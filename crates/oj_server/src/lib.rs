// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::{
    body::Body,
    extract::{ws::Message, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use oj_cache::{CachedModule, PersistentCache};

pub mod optimize;
pub mod pkg_bundle;
pub mod pkg_rolldown;
pub mod plugins;
pub mod sidecar;
pub mod svgr;
use oj_graph::{HmrDecision, ModuleGraph};
use oj_resolver::OjResolver;
use plugins::PluginHost;
use sidecar::{is_tailwind_css, Sidecar};
use tokio::sync::broadcast;

#[inline]
pub fn cobalt(s: &str) -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("\x1b[1;38;2;42;51;212m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

#[inline]
pub fn cell(s: &str) -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m {s} \x1b[0m")
    } else {
        s.to_string()
    }
}

#[inline]
pub fn oj_brand() -> String {
    cell("oj")
}

pub fn link(url: &str, text: &str) -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    } else {
        text.to_string()
    }
}

fn oj_tag() -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("{} ", oj_brand())
    } else {
        "oj:".to_string()
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> std::io::Result<String> {
    match simdutf8::basic::from_utf8(&bytes) {
        Ok(_) => Ok(unsafe { String::from_utf8_unchecked(bytes) }),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )),
    }
}

const CLIENT_JS: &str = include_str!("assets/client.js");
pub const OJ_ROUTES_JS: &str = include_str!("assets/oj-routes.js");
const SERVER_FN_JS: &str = include_str!("assets/server-fn.js");
const LINGUI_MACRO_SHIM_JS: &str = include_str!("assets/lingui-macro-shim.mjs");
const REFRESH_RUNTIME_JS: &str = include_str!("assets/refresh-runtime.js");
const REFRESH_PREAMBLE_JS: &str = include_str!("assets/refresh-preamble.js");
const BUNDLE_RUNTIME_JS: &str = include_str!("assets/bundle-runtime.js");
const WORKER_RUNTIME_JS: &str = include_str!("assets/worker-runtime.js");
pub const SSR_RUNNER_JS: &str = include_str!("assets/ssr-runner.mjs");
const COMPILABLE: &[&str] = &["tsx", "ts", "jsx", "js", "mjs", "svelte"];

const START_ASSETS: &[(&str, &str)] = &[
    (
        "resolve-pkg.mjs",
        include_str!("assets/start/resolve-pkg.mjs"),
    ),
    (
        "rolldown-assets.mjs",
        include_str!("assets/start/rolldown-assets.mjs"),
    ),
    (
        "vite-plugin-bridge.mjs",
        include_str!("assets/start/vite-plugin-bridge.mjs"),
    ),
    (
        "container-bridge.mjs",
        include_str!("assets/start/container-bridge.mjs"),
    ),
    (
        "glob-transform.mjs",
        include_str!("assets/start/glob-transform.mjs"),
    ),
    ("cf-server.mjs", include_str!("assets/start/cf-server.mjs")),
    ("css-host.mjs", include_str!("assets/start/css-host.mjs")),
    ("loader.mjs", include_str!("assets/start/loader.mjs")),
    (
        "loader-util.mjs",
        include_str!("assets/start/loader-util.mjs"),
    ),
    ("runner.mjs", include_str!("assets/start/runner.mjs")),
    ("generate.mjs", include_str!("assets/start/generate.mjs")),
    (
        "gen-resolver.mjs",
        include_str!("assets/start/gen-resolver.mjs"),
    ),
    ("fn-stubs.mjs", include_str!("assets/start/fn-stubs.mjs")),
    (
        "bundle-client.mjs",
        include_str!("assets/start/bundle-client.mjs"),
    ),
    ("build.mjs", include_str!("assets/start/build.mjs")),
    (
        "live-reload.js",
        include_str!("assets/start/live-reload.js"),
    ),
    (
        "server-entry.tsx",
        include_str!("assets/start/server-entry.tsx"),
    ),
    (
        "client-entry.tsx",
        include_str!("assets/start/client-entry.tsx"),
    ),
    (
        "start-entry.ts",
        include_str!("assets/start/start-entry.ts"),
    ),
    (
        "plugin-adapters.ts",
        include_str!("assets/start/plugin-adapters.ts"),
    ),
    ("manifest.ts", include_str!("assets/start/manifest.ts")),
    ("manifest-dev.ts", include_str!("assets/start/manifest-dev.ts")),
];

pub fn boot_phase(label: &str) {
    if std::env::var_os("OJ_BOOT_PHASES").is_none() {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    eprintln!("[oj-phase] {ms} {label}");
}

pub fn node_compile_cache(root: &Path) -> std::ffi::OsString {
    std::env::var_os("NODE_COMPILE_CACHE")
        .unwrap_or_else(|| oj_cache::cache_root(root).join("v8").into_os_string())
}

pub fn node_compile_cache_opt_in(root: &Path) -> Option<std::ffi::OsString> {
    let v = std::env::var_os("OJ_V8_COMPILE_CACHE")?;
    if v.is_empty() || v == "0" {
        return None;
    }
    Some(node_compile_cache(root))
}

pub fn prepare_cache_root(root: &Path) {
    let dir = root.join(".oj-cache");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::write(gitignore, "*\n");
    }
    oj_cache::heal_legacy_layout(root);
    let _ = std::fs::create_dir_all(oj_cache::cache_root(root));
}

pub fn write_start_assets(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, content) in START_ASSETS {
        std::fs::write(dir.join(name), content)?;
    }
    Ok(())
}

pub fn is_tanstack_start_app(root: &Path) -> bool {
    root.join("src/routes").is_dir()
        && std::fs::read_to_string(root.join("package.json"))
            .map(|s| s.contains("@tanstack/react-start"))
            .unwrap_or(false)
}

pub struct DevServer {
    pub root: PathBuf,
    pub port: Option<u16>,
    pub bundle: bool,
    pub host: Option<String>,
    pub config: Option<PathBuf>,
    /// Enable the experimental on-disk module cache (off by default).
    pub enable_cache: bool,
    /// Force the on-disk module cache off even if enabled.
    pub no_cache: bool,
    /// Skip the eager graph crawl; compile modules on demand (Vite's default).
    pub lazy: bool,
}

struct ServerState {
    root: PathBuf,
    public_dir: PathBuf,
    bundle: bool,
    persistent_cache: bool,
    reload_tx: broadcast::Sender<String>,
    graph: Mutex<ModuleGraph>,
    resolver: Arc<OjResolver>,
    ssr_resolver: Arc<OjResolver>,
    cache: PersistentCache,
    memory: Mutex<MemoryCache>,
    mtime_keys: Mutex<HashMap<String, (std::time::SystemTime, u64, String)>>,
    compile_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    crawl_done: tokio::sync::watch::Receiver<bool>,
    fs_allow: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    fs_deny: Vec<(glob::Pattern, bool)>,
    dir_cache: Arc<Mutex<DirCache>>,
    patch_seq: std::sync::atomic::AtomicU64,
    chunk_cache: Mutex<Option<(String, Arc<String>)>>,
    cache_writes: tokio::sync::mpsc::Sender<(String, Arc<CachedModule>)>,
    tailwind: tokio::sync::OnceCell<std::sync::Arc<Sidecar>>,
    preprocess: tokio::sync::OnceCell<std::sync::Arc<Sidecar>>,
    svelte: tokio::sync::OnceCell<std::sync::Arc<Sidecar>>,
    tailwind_urls: Mutex<std::collections::HashSet<String>>,
    has_postcss: bool,
    scss_additional_data: Option<String>,
    sass_additional_data: Option<String>,
    preload_snapshot: Vec<String>,
    proxy: Vec<(String, oj_config::ProxyEntry)>,
    http: reqwest::Client,
    virtual_modules: std::collections::BTreeMap<String, String>,
    jsx_overrides: std::collections::BTreeMap<String, String>,
    hmr_gate: Option<Arc<HmrGate>>,
    plugins: Option<std::sync::Arc<PluginHost>>,
    plugin_mw_port: Option<u16>,
    plugins_ssr: tokio::sync::OnceCell<Option<std::sync::Arc<PluginHost>>>,
    ssr_plugin_config: String,
    plugin_watched: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    plugins_use_module_parsed: bool,
    plugins_have_transform: bool,
    plugins_watch_change: bool,
    plugins_hot_update: bool,
    html_env: std::collections::BTreeMap<String, String>,
    parsed_fired: Mutex<std::collections::HashSet<String>>,
    rt: tokio::runtime::Handle,
    base: Option<String>,
    optimized: Arc<optimize::OptimizedDeps>,
}

pub struct BuiltApp {
    pub router: Router,
    pub host: std::net::IpAddr,
    pub port: u16,
    pub strict_port: bool,
    pub proxy_prefixes: Vec<String>,
    pub plugin_mw_port: Option<u16>,
    pub root: PathBuf,
    pub started: Instant,
    /// Sender for the `/__ws` broadcast — the channel the Lovable editor reads
    /// HMR + narration frames from. The start path pushes narration here.
    pub reload_tx: broadcast::Sender<String>,
}

pub async fn bind_dev_listener(
    host: std::net::IpAddr,
    preferred: u16,
    strict: bool,
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    for port in preferred..=u16::MAX {
        let addr = SocketAddr::from((host, port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                return Ok((listener, bound));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if strict {
                    return Err(anyhow::anyhow!("Port {port} is already in use"));
                }
                eprintln!("Port {port} is in use, trying another one...");
            }
            Err(e) => return Err(e).with_context(|| format!("cannot bind {addr}")),
        }
    }
    Err(anyhow::anyhow!(
        "no available port found between {preferred} and 65535"
    ))
}

impl DevServer {
    pub async fn run(self) -> anyhow::Result<()> {
        let built = self.build_app().await?;
        let (listener, port) = bind_dev_listener(built.host, built.port, built.strict_port).await?;
        println!("  {} dev server", oj_brand());
        println!("  root: {}", built.root.display());
        let url = format!("http://localhost:{}/", port);
        println!("  {}", link(&url, &cell(&url)));
        if !built.proxy_prefixes.is_empty() {
            println!("  proxy: {}", built.proxy_prefixes.join(", "));
        }
        println!("  ready in {:?}", built.started.elapsed());
        axum::serve(listener, built.router).await?;
        Ok(())
    }

    pub async fn build_app(self) -> anyhow::Result<BuiltApp> {
        let root = self
            .root
            .canonicalize()
            .with_context(|| format!("app root not found: {}", self.root.display()))?;

        if let Some(cfg) = &self.config {
            let cfg = if cfg.is_absolute() {
                cfg.clone()
            } else {
                root.join(cfg)
            };
            plugins::set_vite_config_override(cfg);
        }

        boot_phase("build_app begin");
        prepare_cache_root(&root);
        let mut config = oj_config::load(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
        plugins::adopt_vite_config_values(&mut config, &root);
        boot_phase("vite config values adopted");

        // Feed optimizeDeps.include/exclude/needsInterop into partial bundling so
        // the same vite.config field that drives Vite's dep pre-bundle drives oj's.
        {
            let (include, exclude, _entries) = oj_config::optimize_deps_lists(&config);
            pkg_bundle::configure(
                include,
                exclude,
                oj_config::optimize_deps_needs_interop(&config),
            );
        }

        let env_prefixes = oj_config::env_prefixes(&config);
        let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
        let env_dir = config
            .env_dir
            .as_deref()
            .map(|d| root.join(d))
            .unwrap_or_else(|| root.clone());
        let env = oj_env::load(&env_dir, "development");
        let mut defines = oj_env::import_meta_env_defines(
            &env,
            "development",
            true,
            config.base.as_deref().unwrap_or("/"),
            &env_prefix_refs,
        );
        defines.extend(oj_config::config_defines(&config));
        defines.extend(oj_config::environment_defines(&config, "client"));
        defines.extend(oj_config::environment_defines(&config, "ssr"));
        // Vite defines process.env.NODE_ENV in dev too (nodeEnv = NODE_ENV || mode);
        // without it, library code that reads it throws a ReferenceError in dev.
        let node_env = std::env::var("NODE_ENV")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "development".into());
        let node_env_json =
            serde_json::to_string(&node_env).unwrap_or_else(|_| "\"development\"".into());
        for key in [
            "process.env.NODE_ENV",
            "global.process.env.NODE_ENV",
            "globalThis.process.env.NODE_ENV",
        ] {
            if !defines.iter().any(|(k, _)| k == key) {
                defines.push((key.to_string(), node_env_json.clone()));
            }
        }
        let html_env = oj_env::html_env_map(&defines);
        oj_compiler::set_import_meta_env(defines);

        let server_cfg = config.server.clone().unwrap_or_default();
        let port = self.port.or(server_cfg.port).unwrap_or(5199);
        let strict_port = oj_config::server_strict_port(&config);
        let bundle = self.bundle || config.bundle.unwrap_or(false);
        // The on-disk module cache is experimental and off by default. Opt in
        // with `oj dev --enable-cache` (or OJ_ENABLE_CACHE=1); `--no-cache`
        // (or OJ_NO_CACHE=1) forces it off even when otherwise enabled.
        let env_flag = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0");
        let cache_enabled = self.enable_cache || env_flag("OJ_ENABLE_CACHE");
        let cache_forced_off = self.no_cache || env_flag("OJ_NO_CACHE");
        let persistent_cache = cache_enabled && !cache_forced_off;
        let host = resolve_host(self.host.as_deref().or(server_cfg.host.as_deref()));
        let proxy: Vec<(String, oj_config::ProxyEntry)> = server_cfg
            .proxy
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();

        // TanStack Start owns its module graph and SSR; oj runs the plugin host
        // only to host configureServer middleware (the editor dev-server bridge),
        // in start mode so the framework plugins' lifecycle hooks are tolerated.
        let is_start = is_tanstack_start_app(&root);
        let plugin_src = plugins::plugin_source(&root);
        let (plugins_path, plugins_format, plugins_label) = match plugin_src {
            Some(plugins::PluginSource::OjPlugins(p)) => {
                let label = p.file_name().unwrap().to_string_lossy().into_owned();
                (Some(p), "oj", label)
            }
            Some(plugins::PluginSource::ViteConfig(p)) => {
                (Some(p), "vite", "vite.config".to_string())
            }
            None => (None, "oj", String::new()),
        };

        let ssr_bridge_dir = if is_start && plugins_path.is_some() {
            plugins::ensure_ssr_bridge(&root)
        } else {
            None
        };
        if is_start && ssr_bridge_dir.is_none() {
            plugins::disable_ssr_bridge(&root);
        }
        let mut plugin_cfg = serde_json::json!({
            "config": {
                "root": root.display().to_string(),
                "base": config.base.clone().unwrap_or_else(|| "/".into()),
                "mode": "development",
                "command": "serve",
                "define": config.define,
                "server": { "port": port, "host": server_cfg.host },
                "environments": config.environments,
            },
            "env": { "command": "serve", "mode": "development" },
            "environment": { "name": "client", "mode": "dev" },
            "pluginsFormat": plugins_format,
            "ojStartMode": is_start,
        });
        if let Some(dir) = &ssr_bridge_dir {
            plugin_cfg["ssrBridge"] = serde_json::json!({ "dir": dir.display().to_string() });
        }
        let plugin_config = plugin_cfg.to_string();
        plugin_cfg["environment"]["name"] = serde_json::json!("ssr");
        plugin_cfg.as_object_mut().unwrap().remove("ssrBridge");
        let ssr_plugin_config = plugin_cfg.to_string();
        boot_phase("plugin host spawning");
        let plugin_host = match plugins_path {
            Some(file) => match PluginHost::spawn(&root, &file, &plugin_config).await {
                Ok(host) => {
                    // Every remaining plugin may be one oj reimplements natively
                    // (e.g. @vitejs/plugin-react -> oj does JSX/refresh in oxc). If
                    // nothing is left after that filtering, the host is an idle
                    // Node process sitting on the per-request/HMR path -- drop it
                    // and serve natively. Dropping the Arc kills the process.
                    if host.plugin_count().await == 0 {
                        host.shutdown();
                        if ssr_bridge_dir.is_some() {
                            plugins::disable_ssr_bridge(&root);
                        }
                        println!("  plugins: {plugins_label} (none active after native filtering; served natively)");
                        None
                    } else {
                        println!("  plugins: {plugins_label}");
                        if !is_start {
                            if let Err(e) = host.build_start().await {
                                eprintln!("oj: plugin buildStart failed: {e}");
                            }
                        }
                        Some(host)
                    }
                }
                Err(e) => {
                    eprintln!("oj: plugin host failed to start: {e}");
                    if ssr_bridge_dir.is_some() {
                        plugins::disable_ssr_bridge(&root);
                    }
                    None
                }
            },
            None => None,
        };
        boot_phase("plugin host ready");
        let plugin_mw_port = match &plugin_host {
            Some(host) => host.middleware_port().await,
            None => None,
        };
        if let Some(p) = plugin_mw_port {
            println!("  plugin middleware: forwarding unmatched requests to :{p}");
        }
        let plugins_use_module_parsed = match &plugin_host {
            Some(host) => host.has_module_parsed().await,
            None => false,
        };
        // The tagger (and other jsx-override/configureServer plugins) have no
        // transform hook, so the per-module transform RPC is a wasted full-source
        // stdio round-trip; skip it when nothing consumes it.
        let plugins_have_transform = match &plugin_host {
            Some(host) => host.has_transform().await,
            None => false,
        };
        // Same idea for HMR: a host without watchChange/handleHotUpdate hooks (the
        // tagger case) doesn't need those per-save stdio round-trips.
        let (plugins_watch_change, plugins_hot_update) = match &plugin_host {
            Some(host) => host.hmr_hooks().await,
            None => (false, false),
        };

        let jsx_overrides = match &plugin_host {
            Some(host) => resolve_jsx_overrides(host, &root).await,
            None => std::collections::BTreeMap::new(),
        };

        let hmr_gate = {
            let enabled = server_cfg.hmr_gate == Some(true)
                || std::env::var("LOVABLE_DEV_SERVER").as_deref() == Ok("true");
            if enabled {
                let full_reload =
                    std::env::var("LOVABLE_HMR_FULL_RELOAD").as_deref() != Ok("false");
                println!(
                    "  hmr gate: on ({})",
                    if full_reload {
                        "full-reload"
                    } else {
                        "granular"
                    }
                );
                Some(Arc::new(HmrGate {
                    full_reload,
                    max_hold: Duration::from_millis(240_000),
                    inner: Mutex::new(GateInner::default()),
                }))
            } else {
                None
            }
        };

        let started = Instant::now();
        let (reload_tx, _) = broadcast::channel::<String>(64);
        let (crawl_tx, crawl_rx) = tokio::sync::watch::channel(false);
        let (write_tx, mut write_rx) =
            tokio::sync::mpsc::channel::<(String, Arc<CachedModule>)>(65536);
        let public_dir = config
            .public_dir
            .as_ref()
            .map(|p| root.join(p))
            .unwrap_or_else(|| root.join("public"));
        let state = Arc::new(ServerState {
            persistent_cache,
            root: root.clone(),
            public_dir,
            bundle,
            reload_tx: reload_tx.clone(),
            graph: Mutex::new(ModuleGraph::new()),
            resolver: Arc::new(OjResolver::with_settings(
                &root,
                oj_resolver::ResolveSettings {
                    conditions: oj_config::resolve_conditions(&config, "client"),
                    alias: oj_config::resolve_alias(&config, "client"),
                    dedupe: oj_config::resolve_dedupe(&config),
                    extensions: oj_config::resolve_extensions(&config),
                    main_fields: oj_config::resolve_main_fields(&config),
                    preserve_symlinks: oj_config::resolve_preserve_symlinks(&config),
                },
            )),
            ssr_resolver: Arc::new(OjResolver::with_settings(
                &root,
                oj_resolver::ResolveSettings {
                    conditions: oj_config::resolve_conditions(&config, "ssr"),
                    alias: oj_config::resolve_alias(&config, "ssr"),
                    dedupe: oj_config::resolve_dedupe(&config),
                    extensions: oj_config::resolve_extensions(&config),
                    main_fields: oj_config::resolve_main_fields(&config),
                    preserve_symlinks: oj_config::resolve_preserve_symlinks(&config),
                },
            )),
            cache: PersistentCache::new(oj_cache::cache_root(&root), env!("CARGO_PKG_VERSION")),
            memory: Mutex::new(MemoryCache::new(memory_cache_budget())),
            mtime_keys: Mutex::new(HashMap::new()),
            compile_locks: Mutex::new(HashMap::new()),
            crawl_done: crawl_rx,
            tailwind: tokio::sync::OnceCell::new(),
            preprocess: tokio::sync::OnceCell::new(),
            svelte: tokio::sync::OnceCell::new(),
            tailwind_urls: Mutex::new(std::collections::HashSet::new()),
            has_postcss: has_postcss_config(&root),
            scss_additional_data: oj_config::css_additional_data(&config, "scss"),
            sass_additional_data: oj_config::css_additional_data(&config, "sass"),
            fs_allow: Arc::new(Mutex::new(
                server_cfg
                    .fs
                    .as_ref()
                    .and_then(|f| f.allow.as_ref())
                    .map(|allow| {
                        allow
                            .iter()
                            .map(|p| {
                                let pb = PathBuf::from(p);
                                if pb.is_absolute() {
                                    pb
                                } else {
                                    root.join(&pb)
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )),
            fs_deny: compile_fs_deny(&oj_config::server_fs_deny(&config)),
            dir_cache: Arc::new(Mutex::new(DirCache::new())),
            patch_seq: std::sync::atomic::AtomicU64::new(0),
            chunk_cache: Mutex::new(None),
            cache_writes: write_tx,
            preload_snapshot: load_graph_snapshot(&root),
            proxy,
            http: reqwest::Client::new(),
            virtual_modules: config.virtual_modules.clone().unwrap_or_default(),
            jsx_overrides,
            hmr_gate,
            plugins: plugin_host,
            plugin_mw_port,
            plugins_ssr: tokio::sync::OnceCell::new(),
            ssr_plugin_config,
            plugin_watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
            plugins_use_module_parsed,
            plugins_have_transform,
            plugins_watch_change,
            plugins_hot_update,
            html_env,
            parsed_fired: Mutex::new(std::collections::HashSet::new()),
            rt: tokio::runtime::Handle::current(),
            base: config.base.clone().filter(|b| b != "/"),
            optimized: Arc::new(if bundle {
                optimize::OptimizedDeps::disabled()
            } else {
                let (include, exclude, entries) = oj_config::optimize_deps_lists(&config);
                optimize::OptimizedDeps::prepare(
                    &root,
                    env!("CARGO_PKG_VERSION"),
                    optimize::OptimizeInput {
                        include,
                        exclude,
                        entries,
                        dedupe: oj_config::resolve_dedupe(&config),
                        alias: oj_config::resolve_alias(&config, "client"),
                        force: oj_config::optimize_deps_force(&config),
                        bundler_options: oj_config::optimize_deps_bundler_options(&config),
                    },
                )
            }),
        });
        if let Some(host) = &state.plugins {
            host.set_ws_sender(state.reload_tx.clone());
        }
        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                while let Some((key, module)) = write_rx.blocking_recv() {
                    state.cache.put(&key, &module);
                }
            });
        }
        spawn_watcher(Arc::clone(&state));
        if self.lazy {
            // Lazy mode (Vite's default): no eager graph crawl. Modules are
            // compiled on demand as the browser requests them, so the first
            // paint only pays for the first route's modules instead of the whole
            // graph up front. Mark the crawl "done" immediately so preload
            // injection and chunk assembly never block waiting for a crawl that
            // will not run; the module graph still fills in per request.
            let _ = crawl_tx.send(true);
        } else {
            spawn_crawl(Arc::clone(&state), crawl_tx);
        }

        let mut app = Router::new()
            .route("/@oj/client.js", get(|| async { js(CLIENT_JS) }))
            .route(
                "/@oj/refresh-runtime.js",
                get(|| async { js(REFRESH_RUNTIME_JS) }),
            )
            .route(
                "/@oj/refresh-preamble.js",
                get(|| async { js(REFRESH_PREAMBLE_JS) }),
            )
            .route(
                "/@oj/bundle-runtime.js",
                get(|| async { js(BUNDLE_RUNTIME_JS) }),
            )
            .route("/@oj/chunk.js", get(serve_chunk))
            .route("/@oj/patch.js", get(serve_patch))
            .route("/@oj/lazy.js", get(serve_lazy))
            .route("/@oj/worker.js", get(serve_worker_chunk))
            .route("/@oj/routes.js", get(serve_oj_routes))
            .route("/@oj/server-fn.js", get(|| async { js(SERVER_FN_JS) }))
            .route(
                "/@oj/lingui-macro-shim.js",
                get(|| async { js(LINGUI_MACRO_SHIM_JS) }),
            )
            .route("/@ssr-resolve", get(ssr_resolve))
            .route("/@ssr-module", get(ssr_module))
            .route("/__ws", get(ws_upgrade))
            .route("/__hmr_flush", post(hmr_flush))
            .route("/__hmr_gate", get(hmr_gate_status))
            .fallback(serve_fallback);
        let extra_headers: Vec<(header::HeaderName, header::HeaderValue)> = config
            .server
            .as_ref()
            .and_then(|s| s.headers.as_ref())
            .map(|h| {
                h.iter()
                    .filter_map(|(k, v)| Some((k.parse().ok()?, v.parse().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        if !extra_headers.is_empty() {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::new(extra_headers),
                apply_dev_headers,
            ));
        }
        if !state.proxy.is_empty() {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                proxy_middleware,
            ));
        }
        let proxy_prefixes: Vec<String> = state.proxy.iter().map(|(p, _)| p.clone()).collect();
        let app = app.with_state(state);

        Ok(BuiltApp {
            router: app,
            host,
            port,
            strict_port,
            proxy_prefixes,
            plugin_mw_port,
            root,
            started,
            reload_tx,
        })
    }
}

fn js(body: impl IntoResponse) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], body).into_response()
}

async fn ssr_resolve(
    State(state): State<Arc<ServerState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (Some(importer), Some(spec)) = (q.get("importer"), q.get("spec")) else {
        return (StatusCode::BAD_REQUEST, "importer and spec required").into_response();
    };
    let importer_dir = Path::new(importer).parent().unwrap_or(&state.root);
    match state.ssr_resolver.resolve(importer_dir, spec) {
        Ok(p) => {
            let s = p.to_string_lossy();
            let body = if s.contains("/node_modules/") {
                serde_json::json!({ "external": true, "spec": spec })
            } else {
                serde_json::json!({ "id": s })
            };
            js_response_json(body)
        }
        Err(e) => {
            if let Some(host) = ssr_plugin_host(&state).await {
                if let Ok(Some(id)) = host.resolve_id(spec, importer).await {
                    return js_response_json(serde_json::json!({ "id": id }));
                }
            }
            if !spec.starts_with('.') && !spec.starts_with('/') {
                return js_response_json(serde_json::json!({ "external": true, "spec": spec }));
            }
            (
                StatusCode::NOT_FOUND,
                format!("cannot resolve {spec}: {}", e.reason),
            )
                .into_response()
        }
    }
}

fn js_response_json(v: serde_json::Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn module_read_allowed(
    root: &Path,
    allow: &std::collections::HashSet<PathBuf>,
    path: &Path,
) -> bool {
    let Ok(candidate) = std::fs::canonicalize(path) else {
        return true;
    };
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if candidate.starts_with(real(root)) {
        return true;
    }
    if candidate
        .components()
        .any(|c| c.as_os_str() == "node_modules")
    {
        return true;
    }
    allow
        .iter()
        .any(|allowed| candidate.starts_with(real(allowed)))
}

fn ssr_module_allowed(state: &ServerState, path: &Path) -> bool {
    let allow = state.fs_allow.lock().unwrap().clone();
    module_read_allowed(&state.root, &allow, path)
}

async fn ssr_module(
    State(state): State<Arc<ServerState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(id) = q.get("id") else {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    };
    let path = PathBuf::from(id);
    if !ssr_module_allowed(&state, &path) {
        return (StatusCode::FORBIDDEN, "oj: module not allow-listed").into_response();
    }
    let (source, from_plugin) = match std::fs::read(&path).and_then(bytes_to_string) {
        Ok(s) => (s, false),
        Err(read_err) => match ssr_plugin_host(&state).await {
            Some(host) => match host.load(id).await {
                Ok(Some(code)) => (code, true),
                _ => return (StatusCode::NOT_FOUND, format!("{id}: {read_err}")).into_response(),
            },
            None => return (StatusCode::NOT_FOUND, format!("{id}: {read_err}")).into_response(),
        },
    };
    let ext = path.extension().and_then(|e| e.to_str());
    if !from_plugin && ext.is_some_and(is_style_ext) {
        let source = if is_preprocessor(id) {
            match run_preprocess_sidecar(&state, id, &source).await {
                Ok(css) => css,
                Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        } else {
            source
        };
        return match ssr_css_module(&state.root, &path, &source) {
            Ok(code) => js(code),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }
    if !from_plugin && ext == Some("json") {
        return match oj_compiler::json::to_esm(&source, id) {
            Ok(code) => js(code),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
        };
    }
    let source = match ssr_plugin_host(&state).await {
        Some(host) => host
            .transform(&source, id)
            .await
            .map(|(code, _)| code)
            .unwrap_or(source),
        None => source,
    };
    let compile_path: PathBuf = if from_plugin {
        PathBuf::from("virtual.tsx")
    } else {
        path
    };
    match oj_compiler::compile(&compile_path, &source, &oj_compiler::CompileOptions::prod()) {
        Ok(out) => js(out.code),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

async fn ssr_plugin_host(state: &Arc<ServerState>) -> Option<std::sync::Arc<PluginHost>> {
    state
        .plugins_ssr
        .get_or_init(|| async {
            let file = match plugins::plugin_source(&state.root)? {
                plugins::PluginSource::OjPlugins(p) | plugins::PluginSource::ViteConfig(p) => p,
            };
            match PluginHost::spawn(&state.root, &file, &state.ssr_plugin_config).await {
                Ok(host) => {
                    eprintln!("oj ssr: plugins (ssr environment) from {}", file.display());
                    Some(host)
                }
                Err(e) => {
                    eprintln!("oj ssr: plugin host failed to start: {e}");
                    None
                }
            }
        })
        .await
        .clone()
}

fn sass_additional_data_for(state: &ServerState, url: &str) -> Option<String> {
    if !oj_css::is_sass(url) {
        return None;
    }
    let indented = url.split('?').next().unwrap_or(url).ends_with(".sass");
    if indented {
        state.sass_additional_data.clone()
    } else {
        state.scss_additional_data.clone()
    }
}

fn ssr_css_module(root: &Path, path: &Path, source: &str) -> Result<String, String> {
    let css_src = if oj_css::is_sass(&path.to_string_lossy()) {
        oj_css::compile_sass(source, path.parent())?
    } else {
        source.to_string()
    };
    let css_id = match path.strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => path.to_string_lossy().to_string(),
    };
    let output = oj_css::compile_css(&css_id, &css_src, true)?;
    Ok(match output.exports {
        Some(exports) => {
            let map: serde_json::Map<String, serde_json::Value> = exports
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            format!("export default {};", serde_json::Value::Object(map))
        }
        None => "export default {};".to_string(),
    })
}

pub fn resolve_host(host: Option<&str>) -> std::net::IpAddr {
    match host {
        Some("true") | Some("0.0.0.0") | Some("::") | Some("[::]") => [0, 0, 0, 0].into(),
        Some("localhost") | None => [127, 0, 0, 1].into(),
        Some(h) => h.parse().unwrap_or([127, 0, 0, 1].into()),
    }
}

pub async fn preview(
    dir: PathBuf,
    port: u16,
    base: String,
    headers: Vec<(String, String)>,
    host: Option<String>,
) -> anyhow::Result<()> {
    let dir = dir.canonicalize().with_context(|| {
        format!(
            "build dir not found: {} (run `oj build` first)",
            dir.display()
        )
    })?;
    let headers: Vec<(header::HeaderName, header::HeaderValue)> = headers
        .iter()
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.parse().ok()?)))
        .collect();
    let state = Arc::new((dir.clone(), base, headers));
    let app = Router::new().fallback(get(preview_serve)).with_state(state);
    let (listener, port) = bind_dev_listener(resolve_host(host.as_deref()), port, false).await?;
    println!("  {} preview", oj_brand());
    println!("  serving: {}", dir.display());
    let url = format!("http://localhost:{port}/");
    println!("  {}", link(&url, &cell(&url)));
    axum::serve(listener, app).await?;
    Ok(())
}

fn preview_rel(path: &str, base: &str) -> Option<String> {
    let trimmed = path
        .strip_prefix(base.trim_end_matches('/'))
        .unwrap_or(path);
    let rel = urldecode(trimmed.trim_start_matches('/'));
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(if rel.is_empty() {
        "index.html".to_string()
    } else {
        rel
    })
}

async fn preview_serve(
    State(state): State<
        Arc<(
            PathBuf,
            String,
            Vec<(header::HeaderName, header::HeaderValue)>,
        )>,
    >,
    uri: Uri,
) -> Response {
    let (dir, base, extra_headers) = &*state;
    let Some(rel) = preview_rel(uri.path(), base) else {
        return (StatusCode::FORBIDDEN, "oj: path traversal denied").into_response();
    };
    let file = dir.join(&rel);
    let ext = Path::new(&rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let (target, ctype) = if file.is_file() {
        (file, content_type(ext))
    } else if ext.is_empty() {
        (dir.join("index.html"), "text/html; charset=utf-8")
    } else {
        return (StatusCode::NOT_FOUND, format!("oj: not found: {rel}")).into_response();
    };

    // Build assets carry a content hash in their name, so they can be cached
    // forever; HTML is unhashed and must revalidate.
    let cache_control = if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else if ctype.starts_with("text/html") {
        "no-cache"
    } else {
        ""
    };

    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let mut resp = ([(header::CONTENT_TYPE, ctype)], bytes).into_response();
            let h = resp.headers_mut();
            if !cache_control.is_empty() {
                h.insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static(cache_control),
                );
            }
            for (name, value) in extra_headers {
                h.insert(name.clone(), value.clone());
            }
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "oj: not found").into_response(),
    }
}

/// A `lovable:boot-progress` custom HMR frame (editor boot narration). Shape
/// mirrors web/shared/lib/preview/bootProgress.ts; ssrModules + clientModules
/// are required non-negative ints or the editor drops the frame.
pub fn boot_progress_frame(
    ssr_modules: usize,
    client_modules: usize,
    client_idle_ms: Option<u64>,
) -> String {
    serde_json::json!({
        "type": "custom",
        "event": "lovable:boot-progress",
        "data": {
            "ssrModules": ssr_modules,
            "clientModules": client_modules,
            "ssrIdleMs": serde_json::Value::Null,
            "clientIdleMs": client_idle_ms,
            "buildError": serde_json::Value::Null,
        },
    })
    .to_string()
}

/// A `lovable:update-progress` custom HMR frame (editor prompt / steady-state
/// narration). Shape mirrors web/shared/lib/preview/updateProgress.ts; batch is
/// monotonic and trigger is one of "flush" | "watch" | "restart".
pub fn update_progress_frame(
    batch: u64,
    trigger: &str,
    ssr_modules: usize,
    client_modules: usize,
    idle_ms: Option<u64>,
    done: bool,
) -> String {
    serde_json::json!({
        "type": "custom",
        "event": "lovable:update-progress",
        "data": {
            "batch": batch,
            "trigger": trigger,
            "ssrModules": ssr_modules,
            "clientModules": client_modules,
            "idleMs": idle_ms,
            "done": done,
            "buildError": serde_json::Value::Null,
        },
    })
    .to_string()
}

async fn ws_upgrade(
    State(state): State<Arc<ServerState>>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        let mut rx = state.reload_tx.subscribe();
        if state.hmr_gate.is_some() {
            let mode = serde_json::json!({
                "type": "custom",
                "event": "lovable:dev-server-mode",
                "data": { "mode": "classic" },
            })
            .to_string();
            let _ = socket.send(Message::Text(mode.into())).await;
            let boot = boot_progress_frame(0, state.preload_snapshot.len(), Some(0));
            let _ = socket.send(Message::Text(boot.into())).await;
        }
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => handle_client_message(&state, &text),
                    Some(Ok(_)) => {}
                },
            }
        }
    })
}

async fn apply_dev_headers(
    State(headers): State<Arc<Vec<(header::HeaderName, header::HeaderValue)>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    for (name, value) in headers.iter() {
        h.insert(name.clone(), value.clone());
    }
    resp
}

async fn proxy_middleware(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path().to_string();
    let matched = state
        .proxy
        .iter()
        .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len());
    let Some((prefix, entry)) = matched else {
        return next.run(req).await;
    };

    let mut fwd_path = path.clone();
    if let Some((from, to)) = entry.rewrite() {
        if let Some(stripped) = from.strip_prefix('^') {
            if let Some(rest) = fwd_path.strip_prefix(stripped) {
                fwd_path = format!("{to}{rest}");
            }
        } else {
            fwd_path = fwd_path.replacen(from, to, 1);
        }
    }
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!(
        "{}{}{}",
        entry.target().trim_end_matches('/'),
        fwd_path,
        query
    );

    let method = req.method().clone();
    let req_headers = req.headers().clone();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 100 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("oj proxy: body read: {e}")).into_response()
        }
    };

    let mut out = state
        .http
        .request(method, &target)
        .body(body_bytes.to_vec());
    for (name, value) in req_headers.iter() {
        if entry.change_origin() && name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }

    match out.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await.unwrap_or_default();
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = status;
            for (name, value) in headers.iter() {
                if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
                    continue;
                }
                response.headers_mut().insert(name, value.clone());
            }
            response
        }
        Err(e) => {
            let via = if entry.ws() {
                " (ws proxying not yet supported)"
            } else {
                ""
            };
            (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy to {}{} failed: {e}", prefix, via),
            )
                .into_response()
        }
    }
}

async fn serve_html(state: &ServerState, bytes: Vec<u8>) -> Response {
    let mut raw = String::from_utf8_lossy(&bytes).into_owned();
    // %VITE_*% / import.meta.env substitution (Vite's htmlEnvHook), a pre-hook
    // before any plugin transformIndexHtml.
    raw = oj_env::replace_html_env(&raw, &state.html_env);
    if let Some(host) = &state.plugins {
        if let Ok(out) = host.transform_index_html(&raw).await {
            raw = out;
        }
    }
    let html = if state.bundle {
        inject_bundle_scripts(raw)
    } else {
        inject_module_preloads(inject_dev_scripts(raw), state)
    };
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

async fn serve_index_html(state: &ServerState) -> Response {
    match tokio::fs::read(state.root.join("index.html")).await {
        Ok(bytes) => serve_html(state, bytes).await,
        Err(_) => (StatusCode::NOT_FOUND, "oj: index.html not found").into_response(),
    }
}

fn is_spa_navigation(rel: &str, headers: &HeaderMap) -> bool {
    if rel.starts_with('@')
        || rel.starts_with("__")
        || rel.starts_with("src/")
        || rel.starts_with("node_modules/")
    {
        return false;
    }
    let last = rel.rsplit('/').next().unwrap_or("");
    let no_extension = !last.contains('.');
    let accepts_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    no_extension || accepts_html
}

async fn serve_fallback(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
) -> Response {
    if req.method() == Method::GET {
        let headers = req.headers().clone();
        let uri = req.uri().clone();
        return serve_path(State(state), headers, uri).await;
    }
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default()
        .to_vec();
    forward_to_plugin_middleware(&state, &method, &uri, &headers, body)
        .await
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "oj: not found").into_response())
}

async fn forward_to_plugin_middleware(
    state: &ServerState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Option<Response> {
    let port = state.plugin_mw_port?;
    let pq = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
    let target = format!("http://127.0.0.1:{port}{pq}");
    let rmethod = reqwest::Method::from_bytes(method.as_str().as_bytes()).ok()?;
    let mut out = state.http.request(rmethod, &target);
    for (name, value) in headers.iter() {
        if name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }
    if !body.is_empty() {
        out = out.body(body);
    }
    let resp = out.send().await.ok()?;
    if resp.headers().contains_key("x-oj-fallthrough") {
        return None;
    }
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let bytes = resp.bytes().await.unwrap_or_default();
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    for (name, value) in resp_headers.iter() {
        if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().insert(name, value.clone());
    }
    Some(response)
}

// Forward a GET to a plugin's configureServer middleware; returns None when the
// middleware falls through (x-oj-fallthrough), so the caller can fall back to
// SSR. Used by the TanStack start path, where GET requests are otherwise
// SSR'd and would never reach editor endpoints (the dev-server bridge).
pub async fn forward_get_to_plugin_mw(
    port: u16,
    path_and_query: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let target = format!("http://127.0.0.1:{port}{path_and_query}");
    let mut out = client.get(&target);
    for (name, value) in headers.iter() {
        if name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }
    let resp = out.send().await.ok()?;
    if resp.headers().contains_key("x-oj-fallthrough") {
        return None;
    }
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let bytes = resp.bytes().await.unwrap_or_default();
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    for (name, value) in resp_headers.iter() {
        if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().insert(name, value.clone());
    }
    Some(response)
}

async fn serve_path(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let path = state
        .base
        .as_deref()
        .and_then(|b| uri.path().strip_prefix(b.trim_end_matches('/')))
        .unwrap_or_else(|| uri.path());
    let decoded = urldecode(path.trim_start_matches('/'));
    let rel = if decoded.is_empty() {
        "index.html"
    } else {
        decoded.as_str()
    };

    if let Some(name) = uri.path().strip_prefix("/@oj-deps/") {
        if name.contains('/') || name.contains("..") {
            return (StatusCode::FORBIDDEN, "oj: bad optimized dep path").into_response();
        }
        state.optimized.ready().await;
        return match tokio::fs::read(state.optimized.dir().join(name)).await {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => (
                StatusCode::NOT_FOUND,
                format!("oj: no optimized dep {name}"),
            )
                .into_response(),
        };
    }

    if uri.path() == "/@oj-empty" {
        return (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            "// oj: browser-externalized module (package maps it to false)\nexport default {};\nexport const __cjs_exports = {};\n",
        )
            .into_response();
    }

    if uri.path().starts_with(pkg_bundle::PKG_PREFIX) {
        return serve_pkg_bundle(&state, uri.path()).await;
    }

    if let Some(id) = uri.path().strip_prefix("/@virtual/") {
        return match state.virtual_modules.get(id) {
            Some(code) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code.clone(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, format!("oj: no virtual module {id}")).into_response(),
        };
    }

    if let Some(hex) = uri.path().strip_prefix("/@presolve/") {
        let id = hex_decode(hex).unwrap_or_default();
        return serve_plugin_resolve(&state, &id).await;
    }

    if let Some(hex) = uri.path().strip_prefix("/@id/") {
        let spec = hex_decode(hex).unwrap_or_default();
        let importer = uri
            .query()
            .and_then(|q| q.strip_prefix("importer="))
            .and_then(hex_decode)
            .unwrap_or_default();
        return serve_plugin_id(&state, &spec, &importer).await;
    }

    let file = if let Some(abs) = uri.path().strip_prefix("/@fs") {
        let candidate = PathBuf::from(urldecode(abs));
        let allowed = {
            let allow = state.fs_allow.lock().unwrap();
            allow.iter().any(|root| candidate.starts_with(root))
        };
        if !allowed {
            return (StatusCode::FORBIDDEN, "oj: /@fs path not allow-listed").into_response();
        }
        candidate
    } else {
        match locate(&state.root, &state.public_dir, rel) {
            Some(file) => file,
            None => {
                if let Some(resp) =
                    forward_to_plugin_middleware(&state, &Method::GET, &uri, &headers, Vec::new())
                        .await
                {
                    return resp;
                }
                if let Some(resp) = serve_plugin_load_fallback(&state, &uri).await {
                    return resp;
                }
                if is_spa_navigation(rel, &headers) {
                    return serve_index_html(&state).await;
                }
                return (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}"))
                    .into_response();
            }
        }
    };

    if path_is_denied(&file, &state.root, &state.fs_deny) {
        return (StatusCode::FORBIDDEN, "oj: path denied by server.fs.deny").into_response();
    }

    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if let Some(kind) = query_asset_kind(uri.query()) {
        let url = url_of(&state.root, &file);
        return match asset_module(&file, &url, kind).await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }
    if is_style_ext(ext) {
        let import_query = uri.query().is_some_and(|q| q.contains("import"));
        if import_query || !wants_raw_resource(&headers) {
            let url = url_of(&state.root, &file);
            return serve_css_wrapper(&state, &file, &url).await;
        }
    }
    if COMPILABLE.contains(&ext) {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    if ext == "svg"
        && uri
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "react"))
    {
        let url = format!("{}?react", url_of(&state.root, &file));
        return serve_compiled(&state, &file, &url, None, &headers).await;
    }
    if ext == "json" && !file.starts_with(&state.public_dir) {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    if is_importable_asset_ext(ext)
        && query_asset_kind(uri.query()).is_none()
        && !wants_raw_resource(&headers)
    {
        let url = url_of(&state.root, &file);
        return match asset_module(&file, &url, "url").await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }

    match tokio::fs::read(&file).await {
        Ok(bytes) if ext == "html" => serve_html(&state, bytes).await,
        Ok(bytes) if ext == "css" => {
            let source = String::from_utf8_lossy(&bytes).into_owned();
            if is_tailwind_css(&source) {
                let url = url_of(&state.root, &file);
                return match compile_tailwind(&state, &url, &source).await {
                    Ok(css) => (
                        [
                            (header::CONTENT_TYPE, "text/css"),
                            (header::CACHE_CONTROL, "no-cache"),
                        ],
                        css,
                    )
                        .into_response(),
                    Err(err) => {
                        let _ = state.reload_tx.send(
                            serde_json::json!({ "type": "error", "message": err.clone() })
                                .to_string(),
                        );
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
                    }
                };
            }
            ([(header::CONTENT_TYPE, "text/css")], source).into_response()
        }
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type(ext).parse().unwrap());
            response
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: read error: {err}"),
        )
            .into_response(),
    }
}

fn inject_dev_scripts(html: String) -> String {
    let tags = "<script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
                <script type=\"module\" src=\"/@oj/client.js\"></script>";
    match html.find("<head>") {
        Some(idx) => {
            let insert_at = idx + "<head>".len();
            format!("{}\n{}{}", &html[..insert_at], tags, &html[insert_at..])
        }
        None => format!("{tags}\n{html}"),
    }
}

async fn serve_compiled(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    let (key, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            let _ = state
                .reload_tx
                .send(serde_json::json!({ "type": "error", "message": err.clone() }).to_string());
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };

    let etag = format!("\"{key}\"");
    if query.is_none() {
        if let Some(inm) = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
        {
            if inm == etag {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::ETAG, etag),
                        (header::CACHE_CONTROL, "no-cache".to_string()),
                    ],
                )
                    .into_response();
            }
        }
    }

    let mut body = if !state.bundle && module.kind == "svelte" {
        format!("{}{}", svelte_hot_glue(url), module.code)
    } else {
        module.code.clone()
    };
    if !state.bundle && module.kind != "svelte" {
        body.push_str(&hot_glue(url, query, module.is_boundary));
    }
    if let Some(map_url) = &module.map_data_url {
        body.push_str(&format!("\n//# sourceMappingURL={map_url}\n"));
    }

    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

async fn ensure_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<(String, Arc<CachedModule>), String> {
    let react_svg = file.extension().and_then(|e| e.to_str()) == Some("svg")
        && url
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv == "react"));
    let is_svelte = file.extension().and_then(|e| e.to_str()) == Some("svelte");

    if !react_svg && state.bundle {
        if let Some(kind) = query_asset_kind(url.split_once('?').map(|(_, q)| q)) {
            if matches!(kind, "url" | "raw" | "inline" | "init") {
                let code = asset_module(file, url, kind).await?;
                let mut noop = |_: &str| None;
                let factory = oj_compiler::bundle::compile_factory(file, url, &code, &mut noop)
                    .map_err(|err| format!("asset module error for {url}: {err}"))?;
                let module = Arc::new(CachedModule {
                    is_boundary: false,
                    kind: match factory.kind {
                        oj_compiler::bundle::FactoryKind::Esm => "esm".into(),
                        oj_compiler::bundle::FactoryKind::Cjs => "cjs".into(),
                    },
                    code: factory.code,
                    map_data_url: None,
                    imports: factory.imports,
                    require_map: factory.require_map,
                    css_exports: Vec::new(),
                    fs_allow: Vec::new(),
                    watch_files: Vec::new(),
                });
                register_in_graph(state, url, &module);
                return Ok((String::new(), module));
            }
            if matches!(kind, "worker" | "sharedworker") {
                let clean = url.split('?').next().unwrap_or(url);
                let ctor = if kind == "sharedworker" {
                    "SharedWorker"
                } else {
                    "Worker"
                };
                let code = format!(
                    "export default function () {{ return new {ctor}(\"/@oj/worker.js?entry={}\", {{ type: \"module\" }}); }}\n",
                    hex_encode(clean)
                );
                let mut noop = |_: &str| None;
                let factory = oj_compiler::bundle::compile_factory(file, url, &code, &mut noop)
                    .map_err(|err| format!("worker module error for {url}: {err}"))?;
                let module = Arc::new(CachedModule {
                    is_boundary: false,
                    kind: match factory.kind {
                        oj_compiler::bundle::FactoryKind::Esm => "esm".into(),
                        oj_compiler::bundle::FactoryKind::Cjs => "cjs".into(),
                    },
                    code: factory.code,
                    map_data_url: None,
                    imports: factory.imports,
                    require_map: factory.require_map,
                    css_exports: Vec::new(),
                    fs_allow: Vec::new(),
                    watch_files: Vec::new(),
                });
                register_in_graph(state, url, &module);
                return Ok((String::new(), module));
            }
        }
    }

    if !react_svg && is_asset_path(file) {
        let clean = url.split('?').next().unwrap_or(url);
        let default = format!(
            "export default {};\n",
            serde_json::Value::String(clean.to_string())
        );
        let module = if state.bundle {
            let mut noop = |_: &str| None;
            let factory = oj_compiler::bundle::compile_factory(file, url, &default, &mut noop)
                .map_err(|err| format!("asset module error for {url}: {err}"))?;
            Arc::new(CachedModule {
                is_boundary: false,
                kind: match factory.kind {
                    oj_compiler::bundle::FactoryKind::Esm => "esm".into(),
                    oj_compiler::bundle::FactoryKind::Cjs => "cjs".into(),
                },
                code: factory.code,
                map_data_url: None,
                imports: factory.imports,
                require_map: factory.require_map,
                css_exports: Vec::new(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
            })
        } else {
            Arc::new(CachedModule {
                is_boundary: false,
                kind: String::new(),
                code: default,
                map_data_url: None,
                imports: Vec::new(),
                require_map: Vec::new(),
                css_exports: Vec::new(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
            })
        };
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let stamp = match tokio::fs::metadata(file).await {
        Ok(meta) => meta.modified().ok().map(|mtime| (mtime, meta.len())),
        Err(_) => None,
    };
    if let Some((mtime, size)) = stamp {
        let cached_key = state
            .mtime_keys
            .lock()
            .unwrap()
            .get(url)
            .filter(|(t, s, _)| *t == mtime && *s == size)
            .map(|(_, _, k)| k.clone());
        if let Some(key) = cached_key {
            if let Some(module) = memory_get(state, url, &key) {
                register_in_graph(state, url, &module);
                return Ok((key, module));
            }
        }
    }

    let source = bytes_to_string(
        tokio::fs::read(file)
            .await
            .map_err(|err| format!("read error for {url}: {err}"))?,
    )
    .map_err(|err| format!("read error for {url}: {err}"))?;
    if file.extension().and_then(|e| e.to_str()) == Some("css") && is_tailwind_css(&source) {
        let css = compile_tailwind(state, url, &source).await?;
        let module = Arc::new(CachedModule {
            is_boundary: true,
            kind: "css".into(),
            code: css,
            map_data_url: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let is_dep_early = is_dep_module(url, file);
    let is_server = is_server_module(file) && !is_dep_early && !state.bundle;

    let mode = if state.bundle {
        "bundle"
    } else if is_server {
        "server"
    } else {
        "dev"
    };
    let key = state.cache.key(source.as_bytes(), url, mode);
    if let Some((mtime, size)) = stamp {
        state
            .mtime_keys
            .lock()
            .unwrap()
            .insert(url.to_string(), (mtime, size, key.clone()));
    }

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry(url.to_string()).or_default())
    };
    let _guard = lock.lock().await;

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }
    // The persistent (cross-restart) cache holds post-plugin-transform code. A
    // transform can append an import to a plugin-served *virtual* whose content
    // lives only in the plugin's in-memory state: wyw-in-js records each module's
    // extracted CSS in a `cssLookup` its `load` hook serves, and the cached code
    // still `import`s that `.wyw-in-js.css` id. On a warm start the transform
    // never re-runs, so that map is empty and the import 404s. Detect it
    // precisely — a cached module whose imports include a filesystem path with no
    // file on disk depends on such a virtual — and re-run the transform for just
    // those. Modules whose imports are all real files (svgr on disk, plain
    // source, deps) keep the fast persistent cache (Vite has no cross-restart
    // transform cache at all; this preserves oj's where it is sound).
    if let Some(module) = state.persistent_cache.then(|| state.cache.get(&key)).flatten() {
        let module = Arc::new(module);
        let needs_retransform = state.plugins_have_transform
            && !is_dep_early
            && imports_a_plugin_virtual(&module.imports, &state.root, &state.dir_cache);
        if !needs_retransform {
            memory_put(state, url, &key, &module);
            register_in_graph(state, url, &module);
            replay_module_parsed(state, file, &key, is_dep_early, is_server).await;
            return Ok((key, module));
        }
    }

    if is_server {
        let code = server_fn_stub(&oj_compiler::exports(&source, file), url);
        let module = Arc::new(CachedModule {
            is_boundary: false,
            kind: String::new(),
            code,
            map_data_url: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
        });
        if state.persistent_cache {
            let _ = state
                .cache_writes
                .try_send((key.clone(), Arc::clone(&module)));
        }
        memory_put(state, url, &key, &module);
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let is_dep = is_dep_module(url, file);
    let mut plugin_watch_files: Vec<String> = Vec::new();
    let source = match &state.plugins {
        Some(host) if !is_dep && state.plugins_have_transform => {
            match host.transform(&source, &file.to_string_lossy()).await {
                Ok((code, watches)) => {
                    plugin_watch_files = watches;
                    code
                }
                Err(e) => {
                    eprintln!("oj: plugin transform failed for {}: {e}", file.display());
                    source
                }
            }
        }
        _ => source,
    };

    let source = if is_preprocessor(url) {
        run_preprocess_sidecar(state, url, &source)
            .await
            .map_err(|e| format!("css preprocess error for {url}: {e}"))?
    } else {
        source
    };

    let css_like = is_preprocessor(url) || file.extension().and_then(|e| e.to_str()) == Some("css");
    let source = if state.has_postcss && css_like {
        run_css_sidecar(state, url, &source).await.unwrap_or(source)
    } else {
        source
    };

    let source = if react_svg {
        svgr::svg_to_component(&source)
    } else {
        source
    };
    let source = if is_svelte {
        run_svelte_sidecar(state, url, &source)
            .await
            .map_err(|e| format!("svelte compile error for {url}: {e}"))?
    } else {
        source
    };

    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let virtual_ids: std::collections::BTreeSet<String> =
        state.virtual_modules.keys().cloned().collect();
    let jsx_overrides = state.jsx_overrides.clone();
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let file_owned = if react_svg {
        file.with_extension("svg.tsx")
    } else if is_svelte {
        file.with_extension("svelte.js")
    } else {
        file.to_path_buf()
    };
    let url_owned = url.to_string();
    let sass_data = sass_additional_data_for(state, &url_owned);
    let bundle = state.bundle;
    let plugin_fallback = state.plugins.is_some() && !bundle;
    let importer_abs = file.to_string_lossy().into_owned();
    let ext = file.extension().and_then(|e| e.to_str());
    let is_css = ext.is_some_and(is_style_ext);
    let is_json = ext == Some("json");
    let dep_map = if bundle || is_css || is_json {
        Arc::new(optimize::DepMap::new())
    } else {
        state.optimized.ready().await
    };
    let compiled = tokio::task::spawn_blocking(move || -> Result<CachedModule, String> {
        if is_json {
            let code = if bundle {
                oj_compiler::json::to_factory_body(&source, &url_owned)
            } else {
                oj_compiler::json::to_esm(&source, &url_owned)
            }
            .map_err(|err| format!("compile error:\n{err}"))?;
            return Ok(CachedModule {
                is_boundary: false,
                kind: if bundle { "esm".into() } else { String::new() },
                code,
                map_data_url: None,
                imports: Vec::new(),
                require_map: Vec::new(),
                css_exports: Vec::new(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
            });
        }
        if is_css {
            let css_src = if oj_css::is_sass(&url_owned) {
                oj_css::compile_sass_with(&source, Some(&dir), sass_data.as_deref())?
            } else {
                source.clone()
            };
            let output = oj_css::compile_css_rebased(&url_owned, &css_src, false)?;
            return Ok(CachedModule {
                is_boundary: true,
                kind: "css".into(),
                code: output.css,
                map_data_url: None,
                imports: Vec::new(),
                require_map: Vec::new(),
                css_exports: output.exports.unwrap_or_default(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
            });
        }
        let mut rewrite = |spec: &str| {
            if spec == "virtual:oj-routes" {
                return Some("/@oj/routes.js".to_string());
            }
            if virtual_ids.contains(spec) {
                return Some(format!("/@virtual/{spec}"));
            }
            if let Some(id) = jsx_overrides.get(spec) {
                return Some(format!("/@presolve/{}", hex_encode(id)));
            }
            if let Some(meta) = dep_map.get(spec) {
                if !meta.needs_interop {
                    return Some(format!("/@oj-deps/{}", meta.file));
                }
            }
            if let Some(url) =
                rewrite_specifier(&root, &dir, &resolver, &fs_allow, &dir_cache, spec, !bundle)
            {
                return Some(url);
            }
            if plugin_fallback && is_bare_specifier(spec) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            None
        };
        if bundle {
            let factory = oj_compiler::bundle::compile_factory(
                &file_owned,
                &url_owned,
                &source,
                &mut rewrite,
            )
            .map_err(|err| format!("compile error:\n{err}"))?;
            Ok(CachedModule {
                is_boundary: factory.is_boundary(),
                kind: match factory.kind {
                    oj_compiler::bundle::FactoryKind::Esm => "esm".into(),
                    oj_compiler::bundle::FactoryKind::Cjs => "cjs".into(),
                },
                code: factory.code,
                map_data_url: None,
                fs_allow: fs_allow_from(&factory.imports),
                watch_files: Vec::new(),
                imports: factory.imports,
                require_map: factory.require_map,
                css_exports: Vec::new(),
            })
        } else {
            let output = if is_dep {
                oj_compiler::cjs::compile_dep(&file_owned, &url_owned, &source, &mut rewrite)
            } else {
                let interopped =
                    oj_compiler::interop::rewrite_cjs_interop(&source, &file_owned, &|spec| {
                        // lingui macro entrypoints go to the shim (which has real
                        // named exports), never through default-access interop.
                        if is_lingui_macro_specifier(spec) {
                            return None;
                        }
                        if let Some(m) = dep_map.get(spec).filter(|m| m.needs_interop) {
                            return Some(format!("/@oj-deps/{}", m.file));
                        }
                        // A directly-served bare CJS dep (not pre-bundled): rewrite
                        // `import { x } from "dep"` to read x off the default export,
                        // so runtime-assigned CJS exports resolve. Vite pre-bundles
                        // these; oj interops at the importer instead. Restricted to
                        // node_modules so aliased app source (`~/x`, `@/x`, which
                        // is_bare_specifier also matches) is never treated as a dep.
                        if is_bare_specifier(spec) && dep_map.get(spec).is_none() {
                            if let Ok(resolved) = resolver.resolve(&dir, spec) {
                                let in_node_modules = resolved
                                    .components()
                                    .any(|c| c.as_os_str() == "node_modules");
                                // optimizeDeps.needsInterop forces the interop
                                // rewrite even when static analysis reads the dep
                                // as ESM (its real exports only appear at runtime).
                                if in_node_modules
                                    && (is_cjs_dep_file(&resolved)
                                        || pkg_bundle::needs_forced_interop(&resolved))
                                {
                                    fs_allow.lock().unwrap().insert(package_root(&resolved));
                                    // With partial bundling on this is the /@oj-pkg
                                    // bundle URL, which exports __cjs_exports too, so
                                    // the destructured interop still reads names off it.
                                    return Some(dep_serve_url(&resolved, &root));
                                }
                            }
                        }
                        None
                    });
                let opts = if is_svelte {
                    oj_compiler::CompileOptions {
                        dev: true,
                        refresh: false,
                        sourcemap: true,
                    }
                } else {
                    oj_compiler::CompileOptions::dev()
                };
                oj_compiler::compile_module(
                    &file_owned,
                    interopped.as_deref().unwrap_or(&source),
                    &opts,
                    Some(&mut rewrite),
                )
            }
            .map_err(|err| format!("compile error:\n{err}"))?;
            Ok(CachedModule {
                is_boundary: is_svelte || (!is_dep && output.has_refresh_registrations()),
                code: output.code,
                map_data_url: output.map_data_url,
                fs_allow: fs_allow_from(&output.imports),
                watch_files: Vec::new(),
                imports: output.imports,
                kind: if is_svelte {
                    "svelte".into()
                } else {
                    String::new()
                },
                require_map: Vec::new(),
                css_exports: Vec::new(),
            })
        }
    })
    .await;

    let module = match compiled {
        Ok(Ok(mut module)) => {
            module.watch_files = plugin_watch_files;
            Arc::new(module)
        }
        Ok(Err(err)) => return Err(err),
        Err(join_err) => return Err(format!("compiler task failed: {join_err}")),
    };
    if state.persistent_cache {
        let _ = state
            .cache_writes
            .try_send((key.clone(), Arc::clone(&module)));
    }
    memory_put(state, url, &key, &module);
    register_in_graph(state, url, &module);
    if state.plugins_use_module_parsed && !is_dep && !is_server {
        state.parsed_fired.lock().unwrap().insert(key.clone());
    }
    Ok((key, module))
}

async fn replay_module_parsed(
    state: &Arc<ServerState>,
    file: &Path,
    key: &str,
    is_dep: bool,
    is_server: bool,
) {
    if !state.plugins_use_module_parsed || is_dep || is_server {
        return;
    }
    if !state.parsed_fired.lock().unwrap().insert(key.to_string()) {
        return;
    }
    if let Some(host) = &state.plugins {
        let _ = host.module_parsed(&file.to_string_lossy()).await;
    }
}

// Rough fixed cost of one cache entry beyond its module payload: the MemoryEntry
// struct, the HashMap bucket, and the String headers for the url + key.
const MEMORY_ENTRY_OVERHEAD: usize = 160;

struct MemoryEntry {
    key: String,
    module: Arc<CachedModule>,
    bytes: usize,
    seq: u64,
}

struct MemoryCache {
    map: HashMap<String, MemoryEntry>,
    total: usize,
    budget: usize,
    seq: u64,
}

impl MemoryCache {
    fn new(budget: usize) -> Self {
        MemoryCache {
            map: HashMap::new(),
            total: 0,
            budget,
            seq: 0,
        }
    }

    fn get(&mut self, url: &str, key: &str) -> Option<Arc<CachedModule>> {
        self.seq += 1;
        let seq = self.seq;
        let entry = self.map.get_mut(url)?;
        if entry.key != key {
            return None;
        }
        entry.seq = seq;
        Some(Arc::clone(&entry.module))
    }

    fn put(&mut self, url: &str, key: &str, module: &Arc<CachedModule>) {
        let bytes = module_weight(module) + url.len() + key.len() + MEMORY_ENTRY_OVERHEAD;
        self.seq += 1;
        let seq = self.seq;
        let entry = MemoryEntry {
            key: key.to_string(),
            module: Arc::clone(module),
            bytes,
            seq,
        };
        if let Some(old) = self.map.insert(url.to_string(), entry) {
            self.total -= old.bytes;
        }
        self.total += bytes;
        self.evict();
    }

    // Evict in one sorted pass down to a low-water mark (90% of budget) so the
    // hot get()/put() paths stay cheap and eviction runs rarely, not per-put.
    fn evict(&mut self) {
        if self.total <= self.budget {
            return;
        }
        let low = self.budget - self.budget / 10;
        let mut order: Vec<(u64, String)> = self
            .map
            .iter()
            .map(|(url, e)| (e.seq, url.clone()))
            .collect();
        order.sort_unstable_by_key(|(seq, _)| *seq);
        for (_, url) in order {
            if self.total <= low || self.map.len() <= 1 {
                break;
            }
            if let Some(e) = self.map.remove(&url) {
                self.total -= e.bytes;
            }
        }
    }
}

fn module_weight(module: &CachedModule) -> usize {
    fn strs(v: &[String]) -> usize {
        v.iter()
            .map(|s| s.len() + std::mem::size_of::<String>())
            .sum::<usize>()
    }
    fn pairs(v: &[(String, String)]) -> usize {
        v.iter()
            .map(|(a, b)| a.len() + b.len() + 2 * std::mem::size_of::<String>())
            .sum::<usize>()
    }
    module.code.len()
        + module.map_data_url.as_ref().map_or(0, String::len)
        + module.kind.len()
        + strs(&module.imports)
        + strs(&module.fs_allow)
        + strs(&module.watch_files)
        + pairs(&module.require_map)
        + pairs(&module.css_exports)
}

fn memory_cache_budget() -> usize {
    if let Some(mb) = std::env::var("OJ_MEMORY_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return if mb == 0 {
            usize::MAX
        } else {
            mb.saturating_mul(1024 * 1024)
        };
    }
    // No explicit budget: scale to the container memory limit (density) if one is
    // visible, else a sane fixed default for a dev machine.
    let ceil = 256 * 1024 * 1024;
    let floor = 32 * 1024 * 1024;
    match detect_memory_limit() {
        Some(limit) => (limit / 8).clamp(floor, ceil),
        None => 128 * 1024 * 1024,
    }
}

// The cgroup memory ceiling (v2 then v1); None off-container or when unlimited.
fn detect_memory_limit() -> Option<usize> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        let t = s.trim();
        if t != "max" {
            if let Ok(v) = t.parse::<u64>() {
                return Some(v as usize);
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        if let Ok(v) = s.trim().parse::<u64>() {
            if v < (1u64 << 62) {
                return Some(v as usize);
            }
        }
    }
    None
}

fn memory_get(state: &ServerState, url: &str, key: &str) -> Option<Arc<CachedModule>> {
    state.memory.lock().unwrap().get(url, key)
}

fn memory_put(state: &ServerState, url: &str, key: &str, module: &Arc<CachedModule>) {
    state.memory.lock().unwrap().put(url, key, module);
}

fn package_root(path: &Path) -> PathBuf {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("package.json").is_file() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    path.parent().unwrap_or(path).to_path_buf()
}

fn fs_allow_from(imports: &[String]) -> Vec<String> {
    imports
        .iter()
        .filter_map(|i| i.split('?').next().unwrap_or(i).strip_prefix("/@fs"))
        .map(|p| package_root(Path::new(p)).display().to_string())
        .collect()
}

fn register_in_graph(state: &ServerState, url: &str, module: &CachedModule) {
    if !module.fs_allow.is_empty() {
        let mut allow = state.fs_allow.lock().unwrap();
        for p in &module.fs_allow {
            allow.insert(PathBuf::from(p));
        }
    }
    if !module.watch_files.is_empty() {
        let mut watched = state.plugin_watched.lock().unwrap();
        for p in &module.watch_files {
            watched.insert(PathBuf::from(p));
        }
    }
    let mut graph = state.graph.lock().unwrap();
    let local_imports: Vec<PathBuf> = module
        .imports
        .iter()
        .filter(|s| s.starts_with('/') && !s.starts_with("/@oj/") && !is_worker_query(s))
        .map(|s| PathBuf::from(s.split('?').next().unwrap_or(s)))
        .collect();
    graph.set_imports(Path::new(url), &local_imports);
    graph.set_self_accepting(Path::new(url), module.is_boundary);
}

async fn serve_css_wrapper(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    let (_, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            let _ = state
                .reload_tx
                .send(serde_json::json!({ "type": "error", "message": err.clone() }).to_string());
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };
    let exports = if module.css_exports.is_empty() {
        "void 0".to_string()
    } else {
        let map: serde_json::Map<String, serde_json::Value> = module
            .css_exports
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        serde_json::Value::Object(map).to_string()
    };
    let body = format!(
        "import {{ createHotContext as __oj_hot, updateStyle as __oj_updateStyle }} from \"/@oj/client.js\";\n\
         import.meta.hot = __oj_hot({url:?});\n\
         __oj_updateStyle({url:?}, {css});\n\
         export default {exports};\n\
         import.meta.hot.accept(() => {{}});\n",
        css = serde_json::Value::String(module.code.clone()),
    );
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub fn has_postcss_config(root: &Path) -> bool {
    [
        "postcss.config.js",
        "postcss.config.cjs",
        "postcss.config.mjs",
    ]
    .iter()
    .any(|f| root.join(f).is_file())
}

async fn run_css_sidecar(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let sidecar = state
        .tailwind
        .get_or_try_init(|| Sidecar::spawn(&state.root))
        .await
        .map_err(|e| e.to_string())?;
    sidecar.compile(source, url).await
}

fn is_preprocessor(url: &str) -> bool {
    sidecar::is_less(url) || sidecar::is_stylus(url)
}

// Whether a module served from node_modules or /@fs/ is a dependency (routed to
// the dep/CJS-interop path) rather than app/workspace source. A TS/JSX-extension
// file is always source and must be transpiled, even outside the root: monorepo
// packages reached through a resolve.alias are served via /@fs/ but are source.
fn is_dep_module(url: &str, file: &Path) -> bool {
    let src_ext = matches!(
        file.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "jsx" | "mts" | "cts")
    );
    !src_ext && (url.contains("/node_modules/") || url.starts_with("/@fs/"))
}

fn is_style_ext(ext: &str) -> bool {
    matches!(ext, "css" | "scss" | "sass" | "less" | "styl" | "stylus")
}

fn compile_fs_deny(user: &[String]) -> Vec<(glob::Pattern, bool)> {
    const DEFAULTS: &[&str] = &[".env", ".env.*", "*.crt", "*.pem", "**/.git/**"];
    DEFAULTS
        .iter()
        .map(|s| s.to_string())
        .chain(user.iter().cloned())
        .filter_map(|p| {
            let base_only = !p.contains('/');
            glob::Pattern::new(&p).ok().map(|pat| (pat, base_only))
        })
        .collect()
}

fn path_is_denied(file: &Path, root: &Path, deny: &[(glob::Pattern, bool)]) -> bool {
    if deny.is_empty() {
        return false;
    }
    let rel = file.strip_prefix(root).unwrap_or(file);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let abs_str = file.to_string_lossy().replace('\\', "/");
    let base = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (pat, base_only) in deny {
        let hit = if *base_only {
            pat.matches(&base)
        } else {
            pat.matches(&rel_str) || pat.matches(&abs_str)
        };
        if hit {
            return true;
        }
    }
    false
}

// The browser stamps every request with Sec-Fetch-Dest describing what it will
// do with the bytes: `style` (<link rel=stylesheet>), `image` (<img>), `font`
// (@font-face), media. Those want the raw resource. A JS `import` fetches the
// module with dest `script`/`empty`/worker, which wants the JS-module form: a
// style-injecting module for CSS, a URL-exporting module for an asset. Vite draws
// the same line; a `.css` reached from JS is served as JS, not text/css.
fn wants_raw_resource(headers: &HeaderMap) -> bool {
    matches!(
        headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()),
        Some("style" | "image" | "font" | "audio" | "video" | "track" | "object" | "embed")
    )
}

// Assets that, when imported from JS, resolve to a URL-exporting module (Vite's
// default asset handling). svg is excluded: it is routed to vite-plugin-svgr.
fn is_importable_asset_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "bmp"
            | "woff" | "woff2" | "ttf" | "otf" | "eot"
            | "mp4" | "webm" | "ogg" | "mp3" | "wav" | "flac" | "m4a" | "aac" | "mov"
            | "pdf" | "webmanifest"
    )
}

// Node core modules. When one reaches the browser graph (usually via config-time
// tooling a dep drags along), Vite serves a browser-externalized stub rather than
// 404ing the whole module chain; oj does the same so the app still mounts.
fn is_node_builtin(spec: &str) -> bool {
    let name = spec.strip_prefix("node:").unwrap_or(spec);
    let base = name.split('/').next().unwrap_or(name);
    matches!(
        base,
        "assert" | "buffer" | "child_process" | "cluster" | "console" | "constants"
            | "crypto" | "dgram" | "diagnostics_channel" | "dns" | "domain" | "events"
            | "fs" | "http" | "http2" | "https" | "inspector" | "module" | "net" | "os"
            | "path" | "perf_hooks" | "process" | "punycode" | "querystring" | "readline"
            | "repl" | "stream" | "string_decoder" | "sys" | "timers" | "tls" | "trace_events"
            | "tty" | "url" | "util" | "v8" | "vm" | "wasi" | "worker_threads" | "zlib"
    )
}

fn is_style_url(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    std::path::Path::new(f)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_style_ext)
}

async fn run_preprocess_sidecar(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let sidecar = state
        .preprocess
        .get_or_try_init(|| Sidecar::spawn_preprocess(&state.root))
        .await
        .map_err(|e| e.to_string())?;
    sidecar.compile(source, url).await
}

async fn run_svelte_sidecar(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let sidecar = state
        .svelte
        .get_or_try_init(|| Sidecar::spawn_svelte(&state.root))
        .await
        .map_err(|e| e.to_string())?;
    sidecar.compile(source, url).await
}

async fn compile_tailwind(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let css = run_css_sidecar(state, url, source).await?;
    state.tailwind_urls.lock().unwrap().insert(url.to_string());
    Ok(css)
}

fn handle_client_message(state: &Arc<ServerState>, text: &str) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if msg["type"] == "invalidate" {
        let Some(path) = msg["path"].as_str() else {
            return;
        };
        let reply = if state.bundle {
            match state
                .graph
                .lock()
                .unwrap()
                .update_plan_from_importers(Path::new(path))
            {
                Ok(plan) => {
                    println!("oj: invalidate {path} -> patch {:?}", plan.boundaries);
                    let seq = state
                        .patch_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    let to_urls = |v: &[PathBuf]| -> Vec<String> {
                        v.iter().map(|p| p.display().to_string()).collect()
                    };
                    serde_json::json!({
                        "type": "patch",
                        "changed": [],
                        "dirty": to_urls(&plan.dirty),
                        "boundaries": to_urls(&plan.boundaries),
                        "timestamp": now_millis() as u64,
                        "seq": seq,
                    })
                }
                Err(reason) => {
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    serde_json::json!({ "type": "full-reload", "reason": reason })
                }
            }
        } else {
            match state
                .graph
                .lock()
                .unwrap()
                .propagate_update_from_importers(Path::new(path))
            {
                HmrDecision::Update { boundaries } => {
                    println!("oj: invalidate {path} -> update {boundaries:?}");
                    let timestamp = now_millis() as u64;
                    let updates: Vec<_> = boundaries
                        .iter()
                        .map(|b| {
                            serde_json::json!({
                                "path": format!("{}", b.display()),
                                "timestamp": timestamp,
                            })
                        })
                        .collect();
                    serde_json::json!({ "type": "update", "updates": updates })
                }
                HmrDecision::FullReload { reason } => {
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    serde_json::json!({ "type": "full-reload", "reason": reason })
                }
            }
        };
        let _ = state.reload_tx.send(reply.to_string());
    } else if msg["type"] == "custom" {
        if msg["event"].is_string() {
            let _ = state.reload_tx.send(
                serde_json::json!({
                    "type": "custom",
                    "event": msg["event"],
                    "data": msg["data"],
                })
                .to_string(),
            );
            if let Some(host) = state.plugins.clone() {
                let event = msg["event"].as_str().unwrap_or_default().to_string();
                let data = msg["data"].to_string();
                tokio::spawn(async move {
                    let _ = host.ws_message(&event, &data).await;
                });
            }
        }
    }
}

fn hot_glue(url: &str, query: Option<&str>, is_boundary: bool) -> String {
    if !is_boundary {
        return String::new();
    }
    let self_specifier = match query {
        Some(q) if !q.is_empty() => format!("{url}?{q}"),
        _ => url.to_string(),
    };
    format!(
        r#"
import {{ createHotContext as __oj_createHotContext }} from "/@oj/client.js";
import.meta.hot = __oj_createHotContext({url:?});
import * as RefreshRuntime from "/@oj/refresh-runtime.js";
import * as __oj_currentExports from {self_specifier:?};
if (import.meta.hot) {{
  if (!window.__oj_refresh_installed__) {{
    throw new Error("oj: Fast Refresh preamble missing; was index.html served by oj?");
  }}
  const currentExports = __oj_currentExports;
  RefreshRuntime.registerExportsForReactRefresh({url:?}, currentExports);
  import.meta.hot.accept((nextExports) => {{
    if (!nextExports) return;
    const invalidateMessage = RefreshRuntime.validateRefreshBoundaryAndEnqueueUpdate({url:?}, currentExports, nextExports);
    if (invalidateMessage) import.meta.hot.invalidate(invalidateMessage);
  }});
}}
function $RefreshReg$(type, id) {{ return RefreshRuntime.register(type, {url:?} + " " + id); }}
function $RefreshSig$() {{ return RefreshRuntime.createSignatureFunctionForTransform(); }}
"#
    )
}

fn svelte_hot_glue(url: &str) -> String {
    format!(
        "\nimport {{ createHotContext as __oj_createHotContext }} from \"/@oj/client.js\";\nimport.meta.hot = __oj_createHotContext({url:?});\n"
    )
}

type DirCache = std::collections::HashMap<
    PathBuf,
    std::sync::Arc<std::collections::HashMap<std::ffi::OsString, bool>>,
>;

// Whether any of a cached module's imports is a plugin-served virtual — a
// filesystem-path import (not an oj-internal `/@…` route or an external URL)
// with no file on disk. Such a module's transform produced in-memory state
// (e.g. wyw-in-js's extracted CSS) that a warm start would lose, so it must be
// re-transformed. Import URLs are either root-relative (`/src/x.ts`) or absolute
// (`/Users/…/x.wyw-in-js.css`); check both interpretations before concluding a
// path is virtual.
fn imports_a_plugin_virtual(imports: &[String], root: &Path, dir_cache: &Mutex<DirCache>) -> bool {
    imports.iter().any(|imp| {
        let p = imp.split('?').next().unwrap_or(imp);
        if !p.starts_with('/') || p.starts_with("/@") || p.contains("://") {
            return false;
        }
        let as_root = root.join(p.trim_start_matches('/'));
        if is_file_cached(dir_cache, &as_root) {
            return false;
        }
        let as_abs = Path::new(p);
        !is_file_cached(dir_cache, as_abs)
    })
}

fn is_file_cached(cache: &Mutex<DirCache>, path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return path.is_file();
    };
    if let Some(entries) = cache.lock().unwrap().get(dir) {
        return entries.get(name).copied().unwrap_or(false);
    }
    let mut map = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let is_file = match e.file_type() {
                Ok(ft) if ft.is_file() => true,
                Ok(ft) if ft.is_symlink() => e.path().is_file(),
                _ => false,
            };
            map.insert(e.file_name(), is_file);
        }
    }
    let arc = std::sync::Arc::new(map);
    let result = arc.get(name).copied().unwrap_or(false);
    cache.lock().unwrap().insert(dir.to_path_buf(), arc);
    result
}

// Partial bundling (oj-native per-package dep bundling) is opt-in for now.
fn partial_bundle_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OJ_PARTIAL_BUNDLE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

// The URL oj serves a resolved bare dependency from. With partial bundling on, a
// CommonJS package under node_modules is served as a single `/@oj-pkg` bundle
// (keyed by its entry path, so every reference to the same entry — the app's
// import and other packages' requires — collapses to one shared bundle);
// everything else keeps its normal per-file URL. Both the importer-interop and
// specifier-rewrite paths route through here so a dep never gets two URLs.
fn dep_serve_url(resolved: &Path, root: &Path) -> String {
    if partial_bundle_enabled()
        && resolved.components().any(|c| c.as_os_str() == "node_modules")
        && is_bundleable_dep_file(resolved)
        // optimizeDeps.exclude: serve this package per-file, never bundled.
        && !pkg_bundle::is_excluded(resolved)
    {
        return format!(
            "{}{}",
            pkg_bundle::PKG_PREFIX,
            hex_encode(&resolved.to_string_lossy())
        );
    }
    url_of(root, resolved)
}

fn rewrite_specifier(
    root: &Path,
    dir: &Path,
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    dir_cache: &Mutex<DirCache>,
    spec: &str,
    css_import_marker: bool,
) -> Option<String> {
    if spec.starts_with('/') || spec.contains("://") {
        return None;
    }

    if is_lingui_macro_specifier(spec) {
        warn_lingui_macro_shim_once();
        return Some("/@oj/lingui-macro-shim.js".to_string());
    }

    if let Some((base, query)) = spec.split_once('?') {
        if matches!(
            query,
            "url" | "raw" | "inline" | "worker" | "sharedworker" | "init" | "react"
        ) {
            let resolved = rewrite_specifier(root, dir, resolver, fs_allow, dir_cache, base, false)
                .or_else(|| {
                    resolver.resolve(dir, base).ok().map(|p| {
                        fs_allow.lock().unwrap().insert(package_root(&p));
                        url_of(root, &p)
                    })
                })?;
            return Some(format!("{resolved}?{query}"));
        }
    }

    if spec.starts_with("./") || spec.starts_with("../") {
        let mut joined = normalize(&dir.join(spec));
        if !is_file_cached(dir_cache, &joined) {
            if let Some(ext) = joined.extension().and_then(|e| e.to_str()) {
                if ext == "js" || ext == "jsx" {
                    for cand in ["ts", "tsx"] {
                        let alt = joined.with_extension(cand);
                        if is_file_cached(dir_cache, &alt) {
                            joined = alt;
                            break;
                        }
                    }
                }
            }
        }
        let quick = if is_file_cached(dir_cache, &joined) {
            Some(joined)
        } else if joined.extension().is_none() {
            COMPILABLE
                .iter()
                .map(|ext| joined.with_extension(ext))
                .find(|c| is_file_cached(dir_cache, c))
        } else {
            None
        };
        if let Some(p) = quick {
            let url = url_of(root, &p);
            if css_import_marker && is_style_url(&url) {
                return Some(format!("{url}?import"));
            }
            if css_import_marker && is_asset_path(&p) {
                return Some(format!("{url}?url"));
            }
            return Some(url);
        }
    }

    match resolver.resolve(dir, spec) {
        // A node_modules dependency routes through `dep_serve_url` even when it
        // sits under the app root (the common layout), so partial bundling can
        // collapse it. `dep_serve_url` returns the plain per-file URL when partial
        // bundling is off or the file isn't bundleable, so this is a no-op then.
        Ok(resolved) if resolved.components().any(|c| c.as_os_str() == "node_modules") => {
            fs_allow.lock().unwrap().insert(package_root(&resolved));
            Some(dep_serve_url(&resolved, root))
        }
        Ok(resolved) if resolved.starts_with(root) => Some(url_of(root, &resolved)),
        Ok(resolved) => {
            fs_allow.lock().unwrap().insert(package_root(&resolved));
            Some(dep_serve_url(&resolved, root))
        }
        Err(err) if err.ignored => {
            // The package maps this specifier to `false` for the browser (a
            // package.json `browser` field, e.g. typescript's `fs`/`crypto`/
            // `source-map-support`). Vite serves an empty module here; do the
            // same so the importing dep still loads instead of 404ing.
            Some("/@oj-empty".to_string())
        }
        Err(err) => {
            // Plugin-provided ids (virtual: modules, \0-prefixed) are expected
            // to miss the on-disk resolver; the caller's plugin fallback serves
            // them, so a "cannot resolve" line here is just misleading noise.
            let plugin_virtual = spec.starts_with("virtual:") || spec.starts_with('\0');
            if !(spec.starts_with("./") || spec.starts_with("../") || plugin_virtual) {
                eprintln!("oj: cannot resolve '{spec}': {err}");
            }
            None
        }
    }
}

fn url_of(root: &Path, file: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => format!("/@fs{}", file.display()),
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn locate(root: &Path, public_dir: &Path, rel: &str) -> Option<PathBuf> {
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    let base = root.join(rel);
    if base.is_file() {
        return Some(base);
    }
    if base.extension().is_none() {
        for ext in COMPILABLE {
            let candidate = base.with_extension(ext);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let public = public_dir.join(rel);
    if public.is_file() {
        return Some(public);
    }
    None
}

fn is_worker_query(url: &str) -> bool {
    match url.split_once('?') {
        Some((_, q)) => q
            .split('&')
            .any(|kv| kv == "worker" || kv == "sharedworker"),
        None => false,
    }
}

fn is_bundle_asset_query(url: &str) -> bool {
    match url.split_once('?') {
        Some((_, q)) => {
            matches!(
                query_asset_kind(Some(q)),
                Some("url" | "raw" | "inline" | "init" | "worker" | "sharedworker")
            ) || q.split('&').any(|kv| kv == "react")
        }
        None => false,
    }
}

fn query_asset_kind(query: Option<&str>) -> Option<&'static str> {
    let q = query?;
    for kind in ["url", "raw", "inline", "worker", "sharedworker", "init"] {
        if q.split('&').any(|kv| kv == kind) {
            return Some(kind);
        }
    }
    None
}

async fn asset_module(file: &Path, url: &str, kind: &str) -> Result<String, String> {
    let clean_url = url.split('?').next().unwrap_or(url);
    match kind {
        "url" => Ok(format!("export default {clean_url:?};\n")),
        "raw" => {
            let text = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            Ok(format!("export default {};\n", serde_json::Value::String(text)))
        }
        "inline" => {
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if is_style_ext(ext) {
                let raw = tokio::fs::read_to_string(file)
                    .await
                    .map_err(|e| format!("read {}: {e}", file.display()))?;
                let css_src = if oj_css::is_sass(clean_url) {
                    oj_css::compile_sass(&raw, file.parent())?
                } else {
                    raw
                };
                let output = oj_css::compile_css(clean_url, &css_src, false)?;
                return Ok(format!(
                    "export default {};\n",
                    serde_json::Value::String(output.css)
                ));
            }
            let bytes = tokio::fs::read(file).await.map_err(|e| format!("read: {e}"))?;
            let mime = content_type(ext).split(';').next().unwrap_or("application/octet-stream");
            let data_uri = format!("data:{mime};base64,{}", base64_encode(&bytes));
            Ok(format!("export default {data_uri:?};\n"))
        }
        "worker" | "sharedworker" => {
            let ctor = if kind == "sharedworker" { "SharedWorker" } else { "Worker" };
            Ok(format!(
                "export default function () {{ return new {ctor}({clean_url:?}, {{ type: \"module\" }}); }}\n"
            ))
        }
        "init" => Ok(format!(
            "export default (imports = {{}}) => {{\n  const url = {clean_url:?};\n  const inst = (r) => r.instance;\n  const fallback = () => fetch(url).then((r) => r.arrayBuffer()).then((b) => WebAssembly.instantiate(b, imports)).then(inst);\n  if (WebAssembly.instantiateStreaming) {{\n    return WebAssembly.instantiateStreaming(fetch(url), imports).then(inst).catch(fallback);\n  }}\n  return fallback();\n}};\n"
        )),
        _ => Err(format!("unknown asset query: {kind}")),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn is_server_module(file: &Path) -> bool {
    file.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
                .iter()
                .any(|s| n.ends_with(s))
        })
        .unwrap_or(false)
}

fn server_fn_stub(exports: &[String], url: &str) -> String {
    let mut out = String::from("import { __ojServerCall } from \"/@oj/server-fn.js\";\n");
    for name in exports {
        if name == "default" {
            out.push_str(&format!(
                "export default (...a) => __ojServerCall({url:?}, \"default\", a);\n"
            ));
        } else {
            out.push_str(&format!(
                "export const {name} = (...a) => __ojServerCall({url:?}, {name:?}, a);\n"
            ));
        }
    }
    out
}

async fn serve_oj_routes(State(state): State<Arc<ServerState>>) -> Response {
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let synthetic = root.join("oj-routes.tsx");
    let compiled = tokio::task::spawn_blocking(move || {
        let dir = root.clone();
        let mut rewrite =
            |s: &str| rewrite_specifier(&root, &dir, &resolver, &fs_allow, &dir_cache, s, true);
        oj_compiler::compile_module(
            &synthetic,
            OJ_ROUTES_JS,
            &oj_compiler::CompileOptions::dev(),
            Some(&mut rewrite),
        )
        .map(|o| o.code_with_inline_map())
        .map_err(|e| format!("{e}"))
    })
    .await;
    match compiled {
        Ok(Ok(code)) => (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            code,
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: routes manifest: {e}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile task failed: {e}"),
        )
            .into_response(),
    }
}

async fn resolve_jsx_overrides(
    host: &PluginHost,
    root: &Path,
) -> std::collections::BTreeMap<String, String> {
    let mut overrides = std::collections::BTreeMap::new();
    let importer = root.join("index.html");
    let importer = importer.to_string_lossy();
    for spec in ["react/jsx-dev-runtime", "react/jsx-runtime"] {
        if let Ok(Some(id)) = host.resolve_id(spec, &importer).await {
            if id != spec {
                overrides.insert(spec.to_string(), id);
            }
        }
    }
    overrides
}

async fn serve_plugin_resolve(state: &Arc<ServerState>, id: &str) -> Response {
    let Some(host) = &state.plugins else {
        return (StatusCode::NOT_FOUND, "oj: no plugin host").into_response();
    };
    let source = match host.load(id).await {
        Ok(Some(src)) => src,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("oj: no plugin loaded {id}")).into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let dep_map = state.optimized.ready().await;
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let virtual_ids: std::collections::BTreeSet<String> =
        state.virtual_modules.keys().cloned().collect();
    let plugin_fallback = state.plugins.is_some();
    let importer_abs = format!("\0{id}");
    let compiled = tokio::task::spawn_blocking(move || {
        let mut rewrite = |spec: &str| {
            if virtual_ids.contains(spec) {
                return Some(format!("/@virtual/{spec}"));
            }
            if let Some(meta) = dep_map.get(spec) {
                if !meta.needs_interop {
                    return Some(format!("/@oj-deps/{}", meta.file));
                }
            }
            if let Some(url) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, spec, true)
            {
                return Some(url);
            }
            if plugin_fallback && is_bare_specifier(spec) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            None
        };
        oj_compiler::compile_module(
            Path::new("plugin.tsx"),
            &source,
            &oj_compiler::CompileOptions::dev(),
            Some(&mut rewrite),
        )
        .map(|o| o.code_with_inline_map())
        .map_err(|e| format!("{e}"))
    })
    .await;
    match compiled {
        Ok(Ok(code)) => (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            code,
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile task failed: {e}"),
        )
            .into_response(),
    }
}

async fn serve_plugin_id(state: &Arc<ServerState>, spec: &str, importer: &str) -> Response {
    let Some(host) = &state.plugins else {
        return (StatusCode::NOT_FOUND, "oj: no plugin host").into_response();
    };
    let id = match host.resolve_id(spec, importer).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            if is_node_builtin(spec) {
                return (
                    [
                        (header::CONTENT_TYPE, "text/javascript"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    format!("// oj: browser-externalized node builtin {:?}\nexport default {{}};\nexport const __cjs_exports = {{}};\n", spec),
                )
                    .into_response();
            }
            return (
                StatusCode::NOT_FOUND,
                format!("oj: no plugin resolved {spec}"),
            )
                .into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let source = match host.load(&id).await {
        Ok(Some(src)) => src,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("oj: no plugin loaded {id}")).into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let compiled = tokio::task::spawn_blocking(move || {
        let mut rewrite =
            |s: &str| rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, s, true);
        oj_compiler::compile_module(
            Path::new("plugin.tsx"),
            &source,
            &oj_compiler::CompileOptions::dev(),
            Some(&mut rewrite),
        )
        .map(|o| o.code_with_inline_map())
        .map_err(|e| format!("{e}"))
    })
    .await;
    match compiled {
        Ok(Ok(code)) => (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            code,
        )
            .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile task failed: {e}"),
        )
            .into_response(),
    }
}

// A plugin can `load` a module whose id is neither an on-disk file nor a bare
// specifier: wyw-in-js/linaria appends `import "<abs>.wyw-in-js.css"` to each
// transformed module and serves that absolute-path id from its own `load` hook,
// keeping the extracted CSS in memory. On a disk miss, consult the plugin
// container (resolveId -> load) before giving up. CSS a plugin returns is
// wrapped as a style-injecting JS module, matching Vite's `vite:css` handling of
// a `.css` import reached from JS (so the browser gets text/javascript, not a
// text/css module script the strict MIME check rejects).
// Serve a `/@oj-pkg/<hex>` package bundle: one request covering a CommonJS
// package's whole internal file graph (oj-native partial bundling). A package
// that can't be bundled in v1 (ESM entry, unsupported files) falls back to the
// entry's normal per-file compiled output, served at this same URL so the
// importer's interop (which reads __cjs_exports) still resolves.
async fn serve_pkg_bundle(state: &Arc<ServerState>, path: &str) -> Response {
    let js = |code: String| {
        (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            code,
        )
            .into_response()
    };
    // A chunk emitted by a previous rolldown fallback (a code-split sibling, or
    // the entry re-served). These paths aren't decodable entry hexes, so they
    // must be checked before entry_from_url.
    if let Some(code) = pkg_rolldown::cached_chunk(path) {
        return js((*code).clone());
    }
    if let Some(code) = pkg_bundle::cached(path) {
        return js((*code).clone());
    }
    let Some(entry) = pkg_bundle::entry_from_url(path) else {
        return (StatusCode::NOT_FOUND, "oj: bad pkg bundle path").into_response();
    };
    let resolver = Arc::clone(&state.resolver);
    let root = state.root.clone();
    // Known-hard packages (e.g. object-inspect) produce a concatenator bundle
    // that builds but breaks at runtime, so they never bail into the fallback.
    // Force those straight through rolldown, bypassing the concatenator.
    if pkg_rolldown::enabled() && pkg_rolldown::is_forced(&entry) {
        if let Some(code) = pkg_rolldown::build(&entry, &state.root, Arc::clone(&resolver)).await {
            return js((*code).clone());
        }
    }
    let entry_owned = entry.clone();
    let build_resolver = Arc::clone(&resolver);
    let outcome = tokio::task::spawn_blocking(move || {
        pkg_bundle::build(&entry_owned, build_resolver.as_ref(), &root)
    })
    .await;
    match outcome {
        Ok(pkg_bundle::BundleOutcome::Bundle(code)) => {
            let code = Arc::new(code);
            pkg_bundle::store(path, Arc::clone(&code));
            js((*code).clone())
        }
        Ok(pkg_bundle::BundleOutcome::Fallback) => {
            // The concatenator bailed. Before serving per-file, try bundling this
            // one package with rolldown (the robust path, Vite-style), if enabled.
            if pkg_rolldown::enabled() {
                if let Some(code) =
                    pkg_rolldown::build(&entry, &state.root, Arc::clone(&resolver)).await
                {
                    return js((*code).clone());
                }
                if std::env::var("OJ_PB_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0") {
                    eprintln!("oj[pb] rolldown fallback failed, serving per-file: {path}");
                }
            }
            let url = url_of(&state.root, &entry);
            match ensure_module(state, &entry, &url).await {
                Ok((_, module)) => js(module.code.clone()),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: pkg bundle task failed: {e}"),
        )
            .into_response(),
    }
}

async fn serve_plugin_load_fallback(state: &Arc<ServerState>, uri: &Uri) -> Option<Response> {
    let host = state.plugins.as_ref()?;
    let spec = uri.path().to_string();
    let id = match host.resolve_id(&spec, "").await {
        Ok(Some(id)) => id,
        _ => return None,
    };
    let source = match host.load(&id).await {
        Ok(Some(src)) => src,
        _ => return None,
    };
    let id_path = id.split('?').next().unwrap_or(&id);
    let is_css = Path::new(id_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_style_ext);
    if is_css {
        let url = id_path.to_string();
        let body = format!(
            "import {{ createHotContext as __oj_hot, updateStyle as __oj_updateStyle }} from \"/@oj/client.js\";\n\
             import.meta.hot = __oj_hot({url:?});\n\
             __oj_updateStyle({url:?}, {css});\n\
             export default void 0;\n\
             import.meta.hot.accept(() => {{}});\n",
            css = serde_json::Value::String(source),
        );
        return Some(
            (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                body,
            )
                .into_response(),
        );
    }
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let compiled = tokio::task::spawn_blocking(move || {
        let mut rewrite =
            |s: &str| rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, s, true);
        oj_compiler::compile_module(
            Path::new("plugin.tsx"),
            &source,
            &oj_compiler::CompileOptions::dev(),
            Some(&mut rewrite),
        )
        .map(|o| o.code_with_inline_map())
        .map_err(|e| format!("{e}"))
    })
    .await;
    match compiled {
        Ok(Ok(code)) => Some(
            (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code,
            )
                .into_response(),
        ),
        _ => None,
    }
}

fn is_bare_specifier(spec: &str) -> bool {
    !spec.starts_with('.') && !spec.starts_with('/') && !spec.contains("://")
}

// The lingui macro entrypoints. Their transform is done by @lingui/swc-plugin
// (an SWC WASM plugin oj cannot run); left untransformed they drag the babel
// macro toolchain into the browser. oj serves a runtime identity shim instead.
fn is_lingui_macro_specifier(spec: &str) -> bool {
    matches!(
        spec,
        "@lingui/macro" | "@lingui/core/macro" | "@lingui/react/macro"
    )
}

// Whether a resolved dependency file is CommonJS (no ESM syntax), i.e. it will
// be served through wrap_cjs with `default` = module.exports. Used to decide
// whether a bare `import { x } from "cjs-dep"` needs importer-side interop
// (rewriting the named import to a property read off the default), since a CJS
// dep whose named exports are assigned at runtime (e.g. file-saver's `saveAs`)
// exposes no static ESM named bindings. Cached: a file's module kind is stable.
// A node_modules JS-family file that partial bundling should try to collapse
// into one `/@oj-pkg` bundle, whether it's CommonJS or ESM. (`.css`/`.json`/asset
// deps stay per-file; the builder itself falls back if a JS package can't be
// bundled safely.)
fn is_bundleable_dep_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "cjs" | "jsx" | "mjs")
    )
}

fn is_cjs_dep_file(path: &Path) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&v) = cache.lock().unwrap().get(path) {
        return v;
    }
    // Only JavaScript files can be CommonJS. A `.css`/`.json`/asset dep has no
    // ES-module syntax either, but it is not CJS — treating it as one routes it
    // to the CJS interop / package-bundle path and serves raw CSS as JS.
    let is_js = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "cjs" | "jsx")
    );
    let v = is_js
        && match std::fs::read_to_string(path) {
            Ok(src) => !oj_compiler::cjs::has_module_syntax_pub(path, &src),
            Err(_) => false,
        };
    cache.lock().unwrap().insert(path.to_path_buf(), v);
    v
}

fn warn_lingui_macro_shim_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "oj: @lingui/*/macro is served by a runtime identity shim (i18n renders \
             source strings, no catalog lookup). oj cannot run @lingui/swc-plugin, \
             which is what normally compiles these macros."
        );
    });
}

fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

fn hex_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}

fn is_asset_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "ico"
            | "bmp"
            | "svg"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "eot"
            | "mp4"
            | "webm"
            | "mov"
            | "mp3"
            | "wav"
            | "ogg"
            | "wasm"
    )
}

fn is_asset_path(file: &Path) -> bool {
    file.extension()
        .and_then(|e| e.to_str())
        .map(is_asset_ext)
        .unwrap_or(false)
}

fn content_type(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "txt" | "map2" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn inject_module_preloads(html: String, state: &ServerState) -> String {
    let paths: Vec<String> = if *state.crawl_done.borrow() {
        state
            .graph
            .lock()
            .unwrap()
            .module_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    } else {
        state.preload_snapshot.clone()
    };
    if paths.is_empty() {
        return html;
    }
    let links: String = paths
        .iter()
        .map(|p| {
            if is_style_url(p) {
                format!("<link rel=\"modulepreload\" href=\"{p}?import\" />\n")
            } else {
                format!("<link rel=\"modulepreload\" href=\"{p}\" />\n")
            }
        })
        .collect();
    match html.find("</head>") {
        Some(idx) => format!("{}{links}{}", &html[..idx], &html[idx..]),
        None => format!("{html}\n{links}"),
    }
}

fn inject_bundle_scripts(html: String) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(start) = rest.find("<script") {
        let Some(tag_close) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start..start + tag_close];
        let entry_src = tag.contains("type=\"module\"")
            && tag
                .find("src=\"")
                .and_then(|at| tag[at + 5..].split('"').next())
                .and_then(html_entry_src)
                .is_some();
        if entry_src {
            out.push_str(&rest[..start]);
            let after_tag = &rest[start + tag_close + 1..];
            rest = match after_tag.find("</script>") {
                Some(end) => &after_tag[end + "</script>".len()..],
                None => after_tag,
            };
        } else {
            out.push_str(&rest[..start + tag_close + 1]);
            rest = &rest[start + tag_close + 1..];
        }
    }
    out.push_str(rest);

    let tags = "<script type=\"module\" src=\"/@oj/bundle-runtime.js\"></script>\n\
                <script type=\"module\" src=\"/@oj/chunk.js\"></script>";
    match out.find("<head>") {
        Some(idx) => {
            let insert_at = idx + "<head>".len();
            format!("{}\n{}{}", &out[..insert_at], tags, &out[insert_at..])
        }
        None => format!("{tags}\n{out}"),
    }
}

async fn serve_chunk(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if let Some((etag, body)) = state.chunk_cache.lock().unwrap().clone() {
        return chunk_response(&headers, etag, body);
    }

    let mut crawl_done = state.crawl_done.clone();
    if !*crawl_done.borrow() {
        let _ = crawl_done.wait_for(|done| *done).await;
    }
    let urls: Vec<String> = state
        .graph
        .lock()
        .unwrap()
        .module_paths()
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry("/@oj/chunk.js".into()).or_default())
    };
    let _guard = lock.lock().await;
    if let Some((etag, body)) = state.chunk_cache.lock().unwrap().clone() {
        return chunk_response(&headers, etag, body);
    }

    let mut chunk = String::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<String> = urls.into_iter().collect();
    while let Some(url) = queue.pop_front() {
        if !seen.insert(url.clone()) {
            continue;
        }
        let file = match locate_url(&state, &url) {
            Ok(file) => file,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj: chunk: {err}"),
                )
                    .into_response();
            }
        };
        let module = match ensure_module(&state, &file, &url).await {
            Ok((_, module)) => module,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj: chunk: {err}"),
                )
                    .into_response();
            }
        };
        chunk.push_str(&render_registration(&url, &module));
        for imp in &module.imports {
            if is_bundle_asset_query(imp) && !seen.contains(imp) {
                queue.push_back(imp.clone());
            }
        }
    }
    for entry in html_entries(&state.root) {
        chunk.push_str(&format!("__oj_start({entry:?});\n"));
    }
    let etag = format!(
        "\"{}\"",
        state.cache.key(chunk.as_bytes(), "/@oj/chunk.js", "chunk")
    );
    let body = Arc::new(chunk);
    *state.chunk_cache.lock().unwrap() = Some((etag.clone(), Arc::clone(&body)));
    chunk_response(&headers, etag, body)
}

fn chunk_response(headers: &HeaderMap, etag: String, body: Arc<String>) -> Response {
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body.as_str().to_string(),
    )
        .into_response()
}

async fn serve_patch(State(state): State<Arc<ServerState>>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or("");
    let modules = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("m="))
        .map(|v| urldecode(v))
        .unwrap_or_default();

    let mut patch = String::new();
    for url in modules.split(',').filter(|u| !u.is_empty()) {
        match registration_for(&state, url).await {
            Ok(registration) => patch.push_str(&registration),
            Err(err) => {
                let _ = state.reload_tx.send(
                    serde_json::json!({ "type": "error", "message": err.clone() }).to_string(),
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj: patch: {err}"),
                )
                    .into_response();
            }
        }
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        patch,
    )
        .into_response()
}

async fn serve_worker_chunk(State(state): State<Arc<ServerState>>, uri: Uri) -> Response {
    let entry = uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("entry=")))
        .and_then(hex_decode)
        .unwrap_or_default();
    if entry.is_empty() {
        return (StatusCode::BAD_REQUEST, "oj: worker: entry required").into_response();
    }

    let mut chunk = String::from(WORKER_RUNTIME_JS);
    chunk.push('\n');
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue = vec![entry.clone()];
    while let Some(url) = queue.pop() {
        if url.starts_with("/@oj/") || !seen.insert(url.clone()) {
            continue;
        }
        let Ok(file) = locate_url(&state, &url) else {
            continue;
        };
        let module = match ensure_module(&state, &file, &url).await {
            Ok((_, module)) => module,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj: worker: {err}"),
                )
                    .into_response();
            }
        };
        chunk.push_str(&render_registration(&url, &module));
        for imp in &module.imports {
            let next = if is_bundle_asset_query(imp) {
                imp.clone()
            } else {
                imp.split('?').next().unwrap_or(imp).to_string()
            };
            if next.starts_with('/') && !next.starts_with("/@oj/") && !seen.contains(&next) {
                queue.push(next);
            }
        }
    }
    chunk.push_str(&format!(
        "__oj_start({});\n",
        serde_json::Value::String(entry)
    ));
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        chunk,
    )
        .into_response()
}

fn locate_url(state: &ServerState, url: &str) -> Result<PathBuf, String> {
    let base = url.split('?').next().unwrap_or(url);
    if let Some(abs) = base.strip_prefix("/@fs") {
        Ok(PathBuf::from(abs))
    } else {
        let rel = base.trim_start_matches('/');
        locate(&state.root, &state.public_dir, rel).ok_or_else(|| format!("no such module: {url}"))
    }
}

fn render_registration(url: &str, module: &CachedModule) -> String {
    let deps: serde_json::Map<String, serde_json::Value> = module
        .require_map
        .iter()
        .map(|(spec, target)| (spec.clone(), serde_json::Value::String(target.clone())))
        .collect();
    if module.kind == "css" {
        let exports = if module.css_exports.is_empty() {
            "void 0".to_string()
        } else {
            let map: serde_json::Map<String, serde_json::Value> = module
                .css_exports
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            serde_json::Value::Object(map).to_string()
        };
        return format!(
            "__oj_register({url:?}, \"esm\", {{}}, function(module, __oj_exports, __oj_require) {{\n             __oj_esm(__oj_exports, {{ \"default\": () => __oj_css_default }});\n             var __oj_css_default = {exports};\n             __oj_inject_css({url:?}, {css});\n             }});\n",
            css = serde_json::Value::String(module.code.clone()),
        );
    }
    let params = if module.kind == "cjs" {
        "module, exports, require"
    } else {
        "module, __oj_exports, __oj_require"
    };
    format!(
        "__oj_register({url:?}, {kind:?}, {deps}, function({params}) {{\n{body}\n}});\n",
        kind = module.kind,
        deps = serde_json::Value::Object(deps),
        body = module.code,
    )
}

async fn registration_for(state: &Arc<ServerState>, url: &str) -> Result<String, String> {
    let file = locate_url(state, url)?;
    let (_, module) = ensure_module(state, &file, url).await?;
    Ok(render_registration(url, &module))
}

async fn serve_lazy(State(state): State<Arc<ServerState>>, uri: Uri) -> Response {
    let query = uri.query().unwrap_or("");
    let field = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(k))
            .map(urldecode)
    };
    let Some(id) = field("id=").filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "oj: lazy: id required").into_response();
    };
    let mut visited: std::collections::HashSet<String> = field("have=")
        .map(|v| {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut chunk = String::new();
    let start = if is_bundle_asset_query(&id) {
        id.clone()
    } else {
        id.split('?').next().unwrap_or(&id).to_string()
    };
    let mut queue = vec![start];
    while let Some(url) = queue.pop() {
        if url.starts_with("/@oj/") || !visited.insert(url.clone()) {
            continue;
        }
        let Ok(file) = locate_url(&state, &url) else {
            continue;
        };
        let module = match ensure_module(&state, &file, &url).await {
            Ok((_, module)) => module,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj: lazy: {err}"),
                )
                    .into_response()
            }
        };
        chunk.push_str(&render_registration(&url, &module));
        for imp in &module.imports {
            let next = if is_bundle_asset_query(imp) {
                imp.clone()
            } else {
                imp.split('?').next().unwrap_or(imp).to_string()
            };
            if next.starts_with('/') && !next.starts_with("/@oj/") && !visited.contains(&next) {
                queue.push(next);
            }
        }
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        chunk,
    )
        .into_response()
}

fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn html_entry_src(src: &str) -> Option<String> {
    let s = src.trim();
    if s.is_empty()
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("//")
        || s.starts_with("data:")
    {
        return None;
    }
    let s = s.strip_prefix("./").unwrap_or(s);
    Some(if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    })
}

fn html_entries(root: &Path) -> Vec<String> {
    let Ok(html) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for tag_start in html.match_indices("<script").map(|(i, _)| i) {
        let Some(tag_end) = html[tag_start..].find('>') else {
            continue;
        };
        let tag = &html[tag_start..tag_start + tag_end];
        if !tag.contains("type=\"module\"") {
            continue;
        }
        if let Some(src_at) = tag.find("src=\"") {
            let rest = &tag[src_at + 5..];
            if let Some(end) = rest.find('"') {
                if let Some(entry) = html_entry_src(&rest[..end]) {
                    entries.push(entry);
                }
            }
        }
    }
    entries
}

fn spawn_crawl(state: Arc<ServerState>, done_tx: tokio::sync::watch::Sender<bool>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = html_entries(&state.root);
        let mut tasks = tokio::task::JoinSet::new();

        loop {
            for url in queue.drain(..) {
                if !visited.insert(url.clone()) {
                    continue;
                }
                let file = if let Some(abs) = url.strip_prefix("/@fs") {
                    let f = PathBuf::from(abs);
                    let ok = {
                        let a = state.fs_allow.lock().unwrap();
                        a.iter().any(|r| f.starts_with(r))
                    };
                    if !ok {
                        continue;
                    }
                    f
                } else {
                    let rel = url.trim_start_matches('/').to_string();
                    match locate(&state.root, &state.public_dir, &rel) {
                        Some(f) => f,
                        None => continue,
                    }
                };
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !COMPILABLE.contains(&ext) && !(is_style_ext(ext) || ext == "json") {
                    continue;
                }
                let state = Arc::clone(&state);
                tasks.spawn(async move {
                    match ensure_module(&state, &file, &url).await {
                        Ok((_, module)) => module.imports.clone(),
                        Err(err) => {
                            eprintln!("oj: crawl: {err}");
                            Vec::new()
                        }
                    }
                });
            }
            match tasks.join_next().await {
                None => break,
                Some(imports) => {
                    for import in imports.unwrap_or_default() {
                        let import = import.split('?').next().unwrap_or(&import).to_string();
                        if import.starts_with('/')
                            && !import.starts_with("/@oj/")
                            && !visited.contains(&import)
                        {
                            queue.push(import);
                        }
                    }
                }
            }
        }

        let paths = state.graph.lock().unwrap().module_paths();
        println!(
            "{} eager graph ready: {} modules in {:?}",
            oj_tag(),
            paths.len(),
            started.elapsed()
        );
        save_graph_snapshot(&state.root, &paths);
        let _ = done_tx.send(true);
    });
}

fn snapshot_path(root: &Path) -> PathBuf {
    oj_cache::cache_root(&root).join("graph-snapshot.json")
}

fn load_graph_snapshot(root: &Path) -> Vec<String> {
    std::fs::read(snapshot_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_graph_snapshot(root: &Path, paths: &[PathBuf]) {
    let urls: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let path = snapshot_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_vec(&urls).unwrap_or_default());
}

struct HmrGate {
    full_reload: bool,
    max_hold: Duration,
    inner: Mutex<GateInner>,
}

#[derive(Default)]
struct GateInner {
    pending: std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<String>>,
    generation: u64,
}

fn gate_relevant(path: &Path) -> bool {
    !path.components().any(|c| {
        let c = c.as_os_str();
        c == "node_modules" || c == ".oj-cache" || c == "dist"
    })
}

impl HmrGate {
    fn hold(&self, state: &Arc<ServerState>, paths: &[PathBuf]) -> bool {
        let relevant: Vec<&PathBuf> = paths.iter().filter(|p| gate_relevant(p)).collect();
        if relevant.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        let was_empty = inner.pending.is_empty();
        for p in relevant {
            inner
                .pending
                .entry(p.clone())
                .or_default()
                .insert("change".to_string());
        }
        if was_empty {
            inner.generation += 1;
            let generation = inner.generation;
            let state = Arc::clone(state);
            let max_hold = self.max_hold;
            let rt = state.rt.clone();
            rt.spawn(async move {
                tokio::time::sleep(max_hold).await;
                if let Some(gate) = &state.hmr_gate {
                    let expired = {
                        let g = gate.inner.lock().unwrap();
                        g.generation == generation && !g.pending.is_empty()
                    };
                    if expired {
                        gate.flush(&state).await;
                    }
                }
            });
        }
        true
    }

    async fn flush(&self, state: &Arc<ServerState>) -> (Vec<String>, usize) {
        let entries: Vec<(PathBuf, std::collections::BTreeSet<String>)> = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            std::mem::take(&mut inner.pending).into_iter().collect()
        };
        let files: Vec<String> = entries
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        let count = entries.len();
        if !entries.is_empty() {
            *state.chunk_cache.lock().unwrap() = None;
            state.dir_cache.lock().unwrap().clear();
            if self.full_reload {
                let _ = state.reload_tx.send(
                    serde_json::json!({ "type": "full-reload", "reason": "hmr-flush" }).to_string(),
                );
            } else {
                let paths: Vec<PathBuf> = entries.into_iter().map(|(p, _)| p).collect();
                let sref: &ServerState = state;
                for message in decide(sref, &paths).await {
                    let _ = state.reload_tx.send(message);
                }
            }
        }
        (files, count)
    }

    fn mode(&self) -> &'static str {
        if self.full_reload {
            "full-reload"
        } else {
            "granular"
        }
    }

    fn status(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let mut pending = serde_json::Map::new();
        for (p, events) in &inner.pending {
            pending.insert(
                p.display().to_string(),
                serde_json::json!(events.iter().collect::<Vec<_>>()),
            );
        }
        serde_json::json!({
            "enabled": true,
            "pending": pending,
            "count": inner.pending.len(),
            "mode": self.mode(),
        })
    }
}

async fn hmr_flush(State(state): State<Arc<ServerState>>) -> Response {
    let Some(gate) = &state.hmr_gate else {
        return js_response_json(
            serde_json::json!({ "flushed": [], "count": 0, "mode": "disabled" }),
        );
    };
    let (files, count) = gate.flush(&state).await;
    js_response_json(serde_json::json!({ "flushed": files, "count": count, "mode": gate.mode() }))
}

async fn hmr_gate_status(State(state): State<Arc<ServerState>>) -> Response {
    match &state.hmr_gate {
        Some(gate) => js_response_json(gate.status()),
        None => js_response_json(serde_json::json!({ "enabled": false })),
    }
}

/// True for config / env files whose change requires a full server restart
/// (they are read once at startup and cannot be hot-applied).
fn is_restart_trigger(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }
    let stem_ext = |bases: &[&str], exts: &[&str]| {
        bases
            .iter()
            .any(|b| exts.iter().any(|e| name == format!("{b}.{e}")))
    };
    stem_ext(
        &[
            "vite.config",
            "oj.config",
            "postcss.config",
            "tailwind.config",
        ],
        &["ts", "js", "mjs", "cjs", "mts", "cts"],
    )
}

/// Re-exec the current binary with the same arguments so a fresh process
/// re-reads config and .env. Rust sets CLOEXEC on the listening socket, so the
/// dev port is released as the image is replaced. Does not return on success.
fn restart_process() -> ! {
    eprintln!("{} config/env changed — restarting dev server", oj_brand());
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("oj"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(&args).exec();
        eprintln!("oj: restart failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let code = std::process::Command::new(&exe)
            .args(&args)
            .status()
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(0);
        std::process::exit(code);
    }
}

fn spawn_watcher(state: Arc<ServerState>) {
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(err) => {
                eprintln!("oj: file watcher failed to start: {err}");
                return;
            }
        };
        // Watch each top-level entry except node_modules/.oj-cache/dist/.git
        // rather than the whole root: those dirs are huge and, in the case of
        // .oj-cache, rewritten by oj on every compile -- recursively watching
        // them floods the watcher (notably Linux inotify) with self-inflicted
        // events. Skipping them at watch time is more robust than filtering
        // after the fact.
        let ignore = |name: &std::ffi::OsStr| {
            matches!(
                name.to_str(),
                Some("node_modules" | ".oj-cache" | "dist" | ".git")
            )
        };
        let mut watched_any = false;
        if let Ok(entries) = std::fs::read_dir(&state.root) {
            for entry in entries.flatten() {
                if ignore(&entry.file_name()) {
                    continue;
                }
                let path = entry.path();
                let mode = if path.is_dir() {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                if watcher.watch(&path, mode).is_ok() {
                    watched_any = true;
                }
            }
        }
        // Fall back to a recursive root watch only if nothing else could be
        // watched (e.g. an otherwise-empty root).
        if !watched_any {
            if let Err(err) = watcher.watch(&state.root, RecursiveMode::Recursive) {
                eprintln!("oj: cannot watch {}: {err}", state.root.display());
                return;
            }
        }

        use std::sync::mpsc::RecvTimeoutError;
        let debounce_ms: u64 = std::env::var("OJ_HMR_DEBOUNCE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        loop {
            let first = match rx.recv() {
                Ok(Ok(ev)) => ev,
                Ok(Err(_)) => continue,
                Err(_) => break,
            };
            if matches!(first.kind, notify::EventKind::Access(_)) {
                continue;
            }
            let mut paths: std::collections::HashSet<PathBuf> = first.paths.into_iter().collect();
            loop {
                match rx.recv_timeout(Duration::from_millis(debounce_ms)) {
                    Ok(Ok(ev)) if !matches!(ev.kind, notify::EventKind::Access(_)) => {
                        paths.extend(ev.paths);
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let paths: Vec<PathBuf> = paths.into_iter().collect();
            // A config or .env change can't be hot-applied (config is read once at
            // startup), so restart the process to pick it up — matching Vite.
            if paths.iter().any(|p| is_restart_trigger(p)) {
                restart_process();
            }
            if let Some(gate) = &state.hmr_gate {
                if gate.hold(&state, &paths) {
                    continue;
                }
            }
            let messages = state.rt.block_on(decide(&state, &paths));
            if messages.is_empty() {
                continue;
            }
            *state.chunk_cache.lock().unwrap() = None;
            state.dir_cache.lock().unwrap().clear();
            for message in messages {
                let _ = state.reload_tx.send(message);
            }
        }
    });
}

// Async because it is reached both from the watcher thread (via block_on) and
// from the async /__hmr_flush handler; using block_on here panicked ("runtime
// within a runtime") when the gate flushed on an async worker thread.
async fn decide(state: &ServerState, paths: &[PathBuf]) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    let mut updates: Vec<serde_json::Value> = Vec::new();

    let plugin_watched: std::collections::HashSet<PathBuf> = {
        let mut raw: Vec<String> = match &state.plugins {
            Some(host) => host.watch_files().await.unwrap_or_default(),
            None => Vec::new(),
        };
        raw.extend(
            state
                .plugin_watched
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        raw.into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)))
            .collect()
    };

    let source_changed = paths.iter().any(|p| {
        !p.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist"
        }) && p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| COMPILABLE.contains(&e))
    });
    if source_changed {
        let timestamp = now_millis() as u64;
        for url in state.tailwind_urls.lock().unwrap().iter() {
            messages.push(
                serde_json::json!({ "type": "css-update", "path": url, "timestamp": timestamp })
                    .to_string(),
            );
        }
    }

    for path in paths {
        if path.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist"
        }) {
            continue;
        }

        if let Some(host) = &state.plugins {
            let file = path.display().to_string();
            if state.plugins_watch_change {
                let _ = host.watch_change(&file, "update").await;
            }
            if state.plugins_hot_update {
                let ts = now_millis() as u64;
                match host.handle_hot_update(&file, ts).await {
                    Ok(Some(d)) if d == "skip" => {
                        println!("oj: change {file} -> HMR suppressed by plugin");
                        continue;
                    }
                    Ok(Some(d)) if d == "full-reload" => {
                        println!("oj: change {file} -> full-reload (plugin)");
                        messages.push(
                            serde_json::json!({ "type": "full-reload", "reason": "plugin" })
                                .to_string(),
                        );
                        return messages;
                    }
                    _ => {}
                }
            }
        }

        if !plugin_watched.is_empty() {
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if plugin_watched.contains(&canon) {
                println!(
                    "oj: change {} -> full-reload (plugin watch)",
                    path.display()
                );
                messages.push(
                    serde_json::json!({ "type": "full-reload", "reason": "plugin-watch" })
                        .to_string(),
                );
                return messages;
            }
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "css" {
            let url = url_of(&state.root, path);
            if !state.graph.lock().unwrap().contains(Path::new(&url)) {
                println!("oj: change {url} -> css-update");
                messages.push(
                    serde_json::json!({
                        "type": "css-update",
                        "path": url,
                        "timestamp": now_millis() as u64,
                    })
                    .to_string(),
                );
                continue;
            }
        }
        if ext == "html" {
            println!("oj: change {} -> full-reload", path.display());
            messages.push(
                serde_json::json!({ "type": "full-reload", "reason": path.display().to_string() })
                    .to_string(),
            );
            return messages;
        }
        if !COMPILABLE.contains(&ext) && !(is_style_ext(ext) || ext == "json") {
            continue;
        }

        let url = url_of(&state.root, path);
        if !state.graph.lock().unwrap().contains(Path::new(&url)) {
            continue;
        }
        if state.bundle {
            let plan = state.graph.lock().unwrap().update_plan(Path::new(&url));
            match plan {
                Ok(plan) => {
                    println!("oj: change {url} -> patch {:?}", plan.boundaries);
                    let to_urls = |v: &[PathBuf]| -> Vec<String> {
                        v.iter().map(|p| p.display().to_string()).collect()
                    };
                    let seq = state
                        .patch_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    messages.push(
                        serde_json::json!({
                            "type": "patch",
                            "changed": [url],
                            "dirty": to_urls(&plan.dirty),
                            "boundaries": to_urls(&plan.boundaries),
                            "timestamp": now_millis() as u64,
                            "seq": seq,
                        })
                        .to_string(),
                    );
                    continue;
                }
                Err(reason) => {
                    println!("oj: change {url} -> full-reload ({reason})");
                    messages.push(
                        serde_json::json!({ "type": "full-reload", "reason": reason }).to_string(),
                    );
                    return messages;
                }
            }
        }
        let decision = state
            .graph
            .lock()
            .unwrap()
            .propagate_update(Path::new(&url));
        match decision {
            HmrDecision::Update { boundaries } => {
                println!("oj: change {url} -> update {boundaries:?}");
                let timestamp = now_millis() as u64;
                updates.extend(boundaries.iter().map(|b| {
                    let mut path = format!("{}", b.display());
                    if is_style_url(&path) {
                        path.push_str("?import");
                    }
                    serde_json::json!({ "path": path, "timestamp": timestamp })
                }));
            }
            HmrDecision::FullReload { reason } => {
                println!("oj: change {url} -> full-reload ({reason})");
                messages.push(
                    serde_json::json!({ "type": "full-reload", "reason": reason }).to_string(),
                );
                return messages;
            }
        }
    }

    if !updates.is_empty() {
        messages.push(serde_json::json!({ "type": "update", "updates": updates }).to_string());
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_dev_listener_increments_unless_strict() {
        use std::net::{IpAddr, Ipv4Addr};
        let host = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // Hold an ephemeral port so the preferred one is busy.
        let occupied = tokio::net::TcpListener::bind((host, 0)).await.unwrap();
        let taken = occupied.local_addr().unwrap().port();

        // strict: a busy preferred port is a hard error, never moved.
        assert!(
            bind_dev_listener(host, taken, true).await.is_err(),
            "strict must reject a busy port",
        );

        // non-strict (Vite default): hop to the next free port.
        let (listener, port) = bind_dev_listener(host, taken, false).await.unwrap();
        assert_ne!(port, taken, "non-strict must pick a different port");
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[tokio::test]
    async fn css_inline_returns_compiled_css_not_a_data_uri() {
        // `import css from './x.css?inline'` yields the compiled CSS string,
        // not a base64 data: URI (Vite parity).
        let dir = tempfile::tempdir().unwrap();
        let css = dir.path().join("styles.css");
        std::fs::write(&css, ".a { color: red; }").unwrap();
        let out = asset_module(&css, "/styles.css?inline", "inline")
            .await
            .unwrap();
        assert!(out.starts_with("export default \""), "string export: {out}");
        assert!(out.contains("color"), "carries the CSS text: {out}");
        assert!(!out.contains("data:"), "must not be a data URI: {out}");
    }

    #[tokio::test]
    async fn scss_inline_is_compiled_through_sass() {
        let dir = tempfile::tempdir().unwrap();
        let scss = dir.path().join("styles.scss");
        std::fs::write(&scss, "$c: red;\n.a { color: $c; }").unwrap();
        let out = asset_module(&scss, "/styles.scss?inline", "inline")
            .await
            .unwrap();
        assert!(out.contains("red"), "sass variable compiled: {out}");
        assert!(!out.contains("$c"), "sass compiled away: {out}");
    }

    #[tokio::test]
    async fn non_css_inline_stays_a_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("pixel.png");
        std::fs::write(&png, [0u8, 1, 2, 3]).unwrap();
        let out = asset_module(&png, "/pixel.png?inline", "inline")
            .await
            .unwrap();
        assert!(out.contains("data:"), "binary asset stays a data URI: {out}");
    }

    #[test]
    fn fs_deny_blocks_dotenv_and_git_by_default() {
        // No user config: Vite's default deny set still protects secrets.
        let root = Path::new("/proj");
        let deny = compile_fs_deny(&[]);
        assert!(path_is_denied(&root.join(".env"), root, &deny));
        assert!(path_is_denied(&root.join(".env.local"), root, &deny));
        assert!(path_is_denied(&root.join("certs/server.pem"), root, &deny));
        assert!(path_is_denied(&root.join(".git/config"), root, &deny));
        assert!(path_is_denied(&root.join("packages/app/.git/HEAD"), root, &deny));
        // Ordinary source is served.
        assert!(!path_is_denied(&root.join("src/main.tsx"), root, &deny));
        assert!(!path_is_denied(&root.join("src/env.ts"), root, &deny));
    }

    #[test]
    fn fs_deny_honors_user_patterns() {
        let root = Path::new("/proj");
        let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
        assert!(path_is_denied(&root.join("secrets/token.txt"), root, &deny));
        assert!(path_is_denied(&root.join("id_rsa.key"), root, &deny));
        assert!(!path_is_denied(&root.join("public/logo.svg"), root, &deny));
    }

    #[test]
    fn bare_specifier_classification_routes_plugin_virtuals_to_the_fallback() {
        // Plugin virtuals (`virtual:pwa-register`, \0-prefixed ids) count as
        // bare, so the plugin-host fallback (/@id/) resolves them; relative and
        // absolute-URL specifiers do not.
        assert!(is_bare_specifier("react"));
        assert!(is_bare_specifier("virtual:pwa-register"));
        assert!(is_bare_specifier("\0oj-virtual"));
        assert!(!is_bare_specifier("./local"));
        assert!(!is_bare_specifier("../up"));
        assert!(!is_bare_specifier("/abs"));
        assert!(!is_bare_specifier("https://cdn/x.js"));
    }

    #[test]
    fn sec_fetch_dest_decides_raw_vs_module_form() {
        // A `<link>`/`<img>`/@font-face request wants the raw resource; a JS
        // `import` (script/empty dest) wants the JS-module form. This is how a
        // `.css` reached from JS is served as JS, not a text/css module script.
        let raw = |d: &str| {
            let mut h = HeaderMap::new();
            h.insert("sec-fetch-dest", d.parse().unwrap());
            wants_raw_resource(&h)
        };
        assert!(raw("style"));
        assert!(raw("image"));
        assert!(raw("font"));
        assert!(!raw("script"));
        assert!(!raw("empty"));
        // No header (older browsers / non-browser clients): default to the
        // module form so JS imports keep working.
        assert!(!wants_raw_resource(&HeaderMap::new()));
    }

    #[test]
    fn imports_a_plugin_virtual_flags_only_missing_fs_paths() {
        // A cached module is re-transformed on warm start only if it imports a
        // plugin-served virtual: a filesystem-path import with no file on disk.
        // Real files (source, svgr's .svg) and oj-internal /@ routes keep the
        // fast persistent cache.
        let root = std::env::temp_dir().join(format!("oj-ipv-test-{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("real.ts"), "export {}\n").unwrap();
        let dc = Mutex::new(DirCache::new());
        let r = root.as_path();

        // All-real imports -> keep cache.
        assert!(!imports_a_plugin_virtual(&["/src/real.ts".to_string()], r, &dc));
        // oj-internal routes and external urls are never plugin-state virtuals.
        assert!(!imports_a_plugin_virtual(
            &["/@id/abc".to_string(), "/@virtual/x".to_string(), "https://cdn/x.js".to_string()],
            r,
            &dc,
        ));
        // A missing absolute path (wyw's .wyw-in-js.css) -> re-transform.
        assert!(imports_a_plugin_virtual(
            &["/src/real.ts".to_string(), "/Users/nope/x.wyw-in-js.css".to_string()],
            r,
            &dc,
        ));
        // A missing root-relative path -> re-transform.
        assert!(imports_a_plugin_virtual(&["/src/gone.css".to_string()], r, &dc));
        // Query strings are stripped before the on-disk check.
        assert!(!imports_a_plugin_virtual(&["/src/real.ts?import".to_string()], r, &dc));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lingui_macro_specifiers_are_matched_exactly() {
        assert!(is_lingui_macro_specifier("@lingui/macro"));
        assert!(is_lingui_macro_specifier("@lingui/core/macro"));
        assert!(is_lingui_macro_specifier("@lingui/react/macro"));
        // The runtime packages (not the macro entrypoints) must NOT be shimmed.
        assert!(!is_lingui_macro_specifier("@lingui/core"));
        assert!(!is_lingui_macro_specifier("@lingui/react"));
        assert!(!is_lingui_macro_specifier("@lingui/macro/extra"));
    }

    #[test]
    fn node_builtins_are_recognized_for_stubbing() {
        assert!(is_node_builtin("fs"));
        assert!(is_node_builtin("node:fs"));
        assert!(is_node_builtin("fs/promises"));
        assert!(is_node_builtin("crypto"));
        assert!(is_node_builtin("perf_hooks"));
        // Not builtins: real packages and app specifiers.
        assert!(!is_node_builtin("source-map-support"));
        assert!(!is_node_builtin("react"));
        assert!(!is_node_builtin("./local"));
    }

    #[test]
    fn importable_asset_exts_exclude_code_and_svg() {
        assert!(is_importable_asset_ext("webp"));
        assert!(is_importable_asset_ext("png"));
        assert!(is_importable_asset_ext("woff2"));
        // svg is routed to vite-plugin-svgr, not URL-exported here.
        assert!(!is_importable_asset_ext("svg"));
        assert!(!is_importable_asset_ext("css"));
        assert!(!is_importable_asset_ext("js"));
    }

    #[test]
    fn ts_source_outside_root_is_not_treated_as_a_dep() {
        // A monorepo package reached through a resolve.alias is served via /@fs/
        // but is TS/JSX source that must be transpiled, not dep/CJS-interop'd.
        let fs = Path::new("/repo/packages/ui/src/Button.tsx");
        assert!(!is_dep_module("/@fs/repo/packages/ui/src/Button.tsx", fs));
        let ts = Path::new("/repo/packages/ui/src/index.ts");
        assert!(!is_dep_module("/@fs/repo/packages/ui/src/index.ts", ts));
        // A real dependency (.js/.mjs, or anything under node_modules that is not
        // TS/JSX source) stays on the dep path.
        let dep = Path::new("/app/node_modules/react/index.js");
        assert!(is_dep_module("/node_modules/react/index.js", dep));
        let fs_js = Path::new("/repo/packages/ui/dist/index.mjs");
        assert!(is_dep_module("/@fs/repo/packages/ui/dist/index.mjs", fs_js));
        // App-local source (not node_modules, not /@fs/) is never a dep.
        let local = Path::new("/app/src/App.tsx");
        assert!(!is_dep_module("/src/App.tsx", local));
    }

    #[test]
    fn html_injection_puts_preamble_first_in_head() {
        let out = inject_dev_scripts("<html><head><title>x</title></head></html>".into());
        let preamble = out.find("refresh-preamble").unwrap();
        let title = out.find("<title>").unwrap();
        assert!(preamble < title);
    }

    #[test]
    fn glue_only_added_for_boundary_modules() {
        assert!(hot_glue("/src/util.ts", None, false).is_empty());
        let glue = hot_glue("/src/App.tsx", Some("t=123"), true);
        assert!(glue.contains(r#"createHotContext("/src/App.tsx")"#));
        assert!(glue.contains(r#"from "/src/App.tsx?t=123""#), "{glue}");
        assert!(glue.contains("validateRefreshBoundaryAndEnqueueUpdate"));
        assert!(glue.contains("function $RefreshReg$"));
    }

    #[test]
    fn resolve_host_maps_wildcards_to_all_interfaces() {
        let any: std::net::IpAddr = [0, 0, 0, 0].into();
        let local: std::net::IpAddr = [127, 0, 0, 1].into();
        assert_eq!(resolve_host(Some("true")), any);
        assert_eq!(resolve_host(Some("0.0.0.0")), any);
        assert_eq!(resolve_host(Some("::")), any);
        assert_eq!(resolve_host(Some("[::]")), any);
        assert_eq!(resolve_host(Some("localhost")), local);
        assert_eq!(resolve_host(None), local);
        let lan: std::net::IpAddr = [192, 168, 1, 5].into();
        assert_eq!(resolve_host(Some("192.168.1.5")), lan);
        assert_eq!(resolve_host(Some("bogus")), local);
    }

    #[test]
    fn normalize_resolves_parent_components() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
    }

    #[test]
    fn preview_rel_maps_base_and_guards_traversal() {
        assert_eq!(preview_rel("/", "/").as_deref(), Some("index.html"));
        assert_eq!(
            preview_rel("/assets/x.js", "/").as_deref(),
            Some("assets/x.js")
        );
        assert_eq!(
            preview_rel("/app/assets/x.js", "/app/").as_deref(),
            Some("assets/x.js")
        );
        assert_eq!(preview_rel("/app/", "/app/").as_deref(), Some("index.html"));
        assert_eq!(preview_rel("/../etc/passwd", "/"), None);
    }

    #[test]
    fn html_entry_src_normalizes_relative_and_excludes_external() {
        assert_eq!(
            html_entry_src("src/index.tsx").as_deref(),
            Some("/src/index.tsx")
        );
        assert_eq!(
            html_entry_src("./src/index.tsx").as_deref(),
            Some("/src/index.tsx")
        );
        assert_eq!(
            html_entry_src("/src/index.tsx").as_deref(),
            Some("/src/index.tsx")
        );
        assert_eq!(html_entry_src("https://cdn/x.js"), None);
        assert_eq!(html_entry_src("//cdn/x.js"), None);
        assert_eq!(html_entry_src("data:text/js,1"), None);
    }

    #[test]
    fn spa_navigation_falls_back_only_for_routes() {
        let html = {
            let mut h = HeaderMap::new();
            h.insert(
                header::ACCEPT,
                "text/html,application/xhtml+xml".parse().unwrap(),
            );
            h
        };
        let empty = HeaderMap::new();
        assert!(is_spa_navigation("dashboard", &empty));
        assert!(is_spa_navigation("users/123/edit", &empty));
        assert!(is_spa_navigation("report.v2", &html));
        assert!(!is_spa_navigation("missing.png", &empty));
        assert!(!is_spa_navigation("assets/app.js", &empty));
        assert!(!is_spa_navigation("@vite/client", &html));
        assert!(!is_spa_navigation("src/does-not-exist.tsx", &html));
        assert!(!is_spa_navigation("node_modules/react/missing.js", &html));
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    fn tmp(label: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oj-srv-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn is_tanstack_start_app_requires_routes_and_dep() {
        let base = tmp("ts");
        let app = base.join("app");
        std::fs::create_dir_all(app.join("src").join("routes")).unwrap();
        std::fs::write(
            app.join("package.json"),
            r#"{"dependencies":{"react":"19"}}"#,
        )
        .unwrap();
        assert!(!is_tanstack_start_app(&app));
        std::fs::write(
            app.join("package.json"),
            r#"{"dependencies":{"@tanstack/react-start":"1"}}"#,
        )
        .unwrap();
        assert!(is_tanstack_start_app(&app));
        let app2 = base.join("app2");
        std::fs::create_dir_all(app2.join("src")).unwrap();
        std::fs::write(
            app2.join("package.json"),
            r#"{"dependencies":{"@tanstack/react-start":"1"}}"#,
        )
        .unwrap();
        assert!(!is_tanstack_start_app(&app2));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_ssr_module_endpoint_only_reads_the_project_and_its_dependencies() {
        let base = tmp("ssr-allow");
        let root = base.join("app");
        let outside = base.join("elsewhere");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/react")).unwrap();
        std::fs::create_dir_all(outside.join("secrets")).unwrap();
        std::fs::create_dir_all(base.join("linked/node_modules/dep")).unwrap();
        std::fs::create_dir_all(base.join("allowed")).unwrap();
        std::fs::write(root.join("src/App.tsx"), "x").unwrap();
        std::fs::write(root.join("node_modules/react/index.js"), "x").unwrap();
        std::fs::write(outside.join("secrets/id_rsa"), "x").unwrap();
        std::fs::write(base.join("linked/node_modules/dep/index.js"), "x").unwrap();
        std::fs::write(base.join("allowed/shared.ts"), "x").unwrap();

        let mut allow = std::collections::HashSet::new();
        allow.insert(base.join("allowed"));

        // The project, its dependencies, and what `server.fs.allow` named.
        for ok in [
            root.join("src/App.tsx"),
            root.join("node_modules/react/index.js"),
            base.join("linked/node_modules/dep/index.js"),
            base.join("allowed/shared.ts"),
        ] {
            assert!(module_read_allowed(&root, &allow, &ok), "{ok:?} denied");
        }

        // Everything else, however it is spelled.
        for denied in [
            outside.join("secrets/id_rsa"),
            root.join("../elsewhere/secrets/id_rsa"),
            root.join("src/../../elsewhere/secrets/id_rsa"),
        ] {
            assert!(
                !module_read_allowed(&root, &allow, &denied),
                "{denied:?} allowed"
            );
        }

        // A virtual id is not a filesystem read: the plugin host is the only
        // thing that can resolve it, so it passes through.
        for virtual_id in ["virtual:oj-routes", "\0virtual:x", "plugin:generated"] {
            assert!(module_read_allowed(&root, &allow, Path::new(virtual_id)));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_does_not_widen_what_can_be_read() {
        let base = tmp("ssr-symlink");
        let root = base.join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(base.join("secrets")).unwrap();
        std::fs::write(base.join("secrets/id_rsa"), "x").unwrap();
        let link = root.join("src/escape.ts");
        if std::os::unix::fs::symlink(base.join("secrets/id_rsa"), &link).is_ok() {
            assert!(
                !module_read_allowed(&root, &std::collections::HashSet::new(), &link),
                "a symlink inside the project must not expose its target"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_prefers_root_then_public_dir() {
        let base = tmp("locate");
        let root = base.join("root");
        let public = base.join("shared-public");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(public.join("img")).unwrap();
        std::fs::write(root.join("src").join("App.tsx"), "x").unwrap();
        std::fs::write(public.join("img").join("logo.webp"), "y").unwrap();

        assert_eq!(
            locate(&root, &public, "src/App"),
            Some(root.join("src/App.tsx"))
        );
        assert_eq!(
            locate(&root, &public, "img/logo.webp"),
            Some(public.join("img/logo.webp"))
        );
        assert_eq!(locate(&root, &public, "img/missing.webp"), None);
        assert_eq!(locate(&root, &public, "../secret"), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- adverse: request paths are attacker-shaped strings ---

    #[test]
    fn urldecode_handles_every_malformed_escape() {
        assert_eq!(urldecode("plain/path.tsx"), "plain/path.tsx");
        assert_eq!(urldecode("a%20b"), "a b");
        assert_eq!(urldecode("a%2Fb"), "a/b");
        assert_eq!(urldecode("caf%C3%A9.css"), "café.css");
        assert_eq!(urldecode("100%25.css"), "100%.css");
        // Truncated and non-hex escapes are left alone rather than dropped.
        assert_eq!(urldecode("a%"), "a%");
        assert_eq!(urldecode("a%2"), "a%2");
        assert_eq!(urldecode("a%zz"), "a%zz");
        assert_eq!(urldecode("a%%20"), "a% ");
        assert_eq!(urldecode(""), "");
        // A single pass only: an encoded escape stays encoded once decoded.
        assert_eq!(urldecode("a%2520b"), "a%20b");
        // Invalid UTF-8 becomes replacement characters instead of panicking.
        assert!(!urldecode("%ff%fe").is_empty());
    }

    #[test]
    fn locate_rejects_traversal_in_every_spelling_after_decoding() {
        let base = tmp("traversal");
        let root = base.join("root");
        let public = base.join("public");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(base.join("secret.txt"), "s3cret").unwrap();
        std::fs::write(root.join("src").join("App.tsx"), "x").unwrap();

        for hostile in [
            "../secret.txt",
            "src/../../secret.txt",
            "./../secret.txt",
            "src/../../../etc/passwd",
        ] {
            assert_eq!(locate(&root, &public, hostile), None, "{hostile}");
            // ...and through the decoding the request handler applies first.
            let encoded = hostile.replace("..", "%2e%2e");
            assert_eq!(
                locate(&root, &public, &urldecode(&encoded)),
                None,
                "{encoded}"
            );
        }
        // A name that merely contains dots is not traversal.
        std::fs::write(root.join("src").join("..dotted.tsx"), "x").unwrap();
        assert!(locate(&root, &public, "src/..dotted.tsx").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_finds_files_whose_names_need_encoding() {
        let base = tmp("encoded-names");
        let root = base.join("root");
        let public = base.join("public");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&public).unwrap();
        for name in ["Cool Button.tsx", "café.css", "100%.css", "a+b.tsx"] {
            std::fs::write(root.join("src").join(name), "x").unwrap();
        }

        // What a browser actually requests for each of those files.
        for (requested, name) in [
            ("src/Cool%20Button.tsx", "Cool Button.tsx"),
            ("src/caf%C3%A9.css", "café.css"),
            ("src/100%25.css", "100%.css"),
            ("src/a+b.tsx", "a+b.tsx"),
        ] {
            let decoded = urldecode(requested);
            assert_eq!(
                locate(&root, &public, &decoded),
                Some(root.join("src").join(name)),
                "{requested}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preview_rel_decodes_before_guarding_traversal() {
        assert_eq!(
            preview_rel("/assets/Cool%20Button.js", "/").as_deref(),
            Some("assets/Cool Button.js")
        );
        assert_eq!(preview_rel("/%2e%2e/etc/passwd", "/"), None);
        assert_eq!(preview_rel("/a/%2e%2e/%2e%2e/etc/passwd", "/"), None);
        assert_eq!(preview_rel("/%2E%2E/etc/passwd", "/"), None);
        // Not traversal: `..` inside a segment.
        assert_eq!(
            preview_rel("/a..b/x.js", "/").as_deref(),
            Some("a..b/x.js")
        );
    }

    #[test]
    fn html_entry_src_rejects_everything_that_is_not_a_local_path() {
        for external in [
            "",
            "   ",
            "http://cdn.example/x.js",
            "https://cdn.example/x.js",
            "//cdn.example/x.js",
            "data:text/javascript,alert(1)",
        ] {
            assert_eq!(html_entry_src(external), None, "{external:?}");
        }
        assert_eq!(html_entry_src("./src/main.tsx").as_deref(), Some("/src/main.tsx"));
        assert_eq!(html_entry_src("src/main.tsx").as_deref(), Some("/src/main.tsx"));
        assert_eq!(html_entry_src("/src/main.tsx").as_deref(), Some("/src/main.tsx"));
        assert_eq!(html_entry_src("  src/main.tsx  ").as_deref(), Some("/src/main.tsx"));
        // A traversal in an entry src stays a path; `locate` is what refuses it.
        assert_eq!(
            html_entry_src("../../etc/passwd").as_deref(),
            Some("/../../etc/passwd")
        );
    }

    #[test]
    fn gate_relevance_ignores_generated_directories() {
        assert!(gate_relevant(Path::new("/app/src/App.tsx")));
        assert!(!gate_relevant(Path::new("/app/node_modules/react/index.js")));
        assert!(!gate_relevant(Path::new("/app/.oj-cache/ab/cd.json")));
        assert!(!gate_relevant(Path::new("/app/dist/assets/x.js")));
        // Only a whole path component counts.
        assert!(gate_relevant(Path::new("/app/src/dist-helper.ts")));
        assert!(gate_relevant(Path::new("/app/src/my-node_modules-thing.ts")));
    }

    #[test]
    fn query_classification_only_matches_whole_flags() {
        assert!(is_worker_query("/w.ts?worker"));
        assert!(is_worker_query("/w.ts?sharedworker"));
        assert!(!is_worker_query("/w.ts?workerish"));
        assert!(!is_worker_query("/w.ts?x=worker"));
        assert!(!is_worker_query("/w.ts"));
        assert!(!is_worker_query(""));
    }

    #[test]
    fn is_spa_navigation_rules() {
        let empty = HeaderMap::new();
        assert!(is_spa_navigation("dashboard", &empty));
        assert!(is_spa_navigation("projects/abc", &empty));
        assert!(!is_spa_navigation("main.js", &empty));
        assert!(!is_spa_navigation("@vite/client", &empty));
        assert!(!is_spa_navigation("src/App.tsx", &empty));
        assert!(!is_spa_navigation("node_modules/react/index.js", &empty));
        let mut html = HeaderMap::new();
        html.insert(header::ACCEPT, "text/html,*/*".parse().unwrap());
        assert!(is_spa_navigation("some.thing", &html));
    }
}
