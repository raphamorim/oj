// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    extract::{ws::Message, FromRequestParts, Request, State, WebSocketUpgrade},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::broadcast;

struct Runner {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    _child: Child,
}

struct StartState {
    proxy_prefixes: Vec<String>,
    plugin_mw_port: Option<u16>,
    runner: Arc<tokio::sync::Mutex<Runner>>,
    bundle: std::sync::RwLock<Arc<oj_cache::start_bundle::PinnedBundle>>,
    verify: oj_cache::integrity::VerifyMode,
    live_reload: PathBuf,
    workspace_root: PathBuf,
    reload_tx: broadcast::Sender<()>,
    // The oj_server /__ws broadcast: the channel the Lovable editor reads boot +
    // update narration frames from (the start path's own /@oj-start/hmr socket
    // only drives the app iframe's live reload).
    ws_tx: broadcast::Sender<String>,
    css_host: Option<Arc<tokio::sync::Mutex<Runner>>>,
    worker_url: Option<String>,
}

pub async fn start_dev(
    root: PathBuf,
    port: Option<u16>,
    host: Option<String>,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
    oj_server::plugins::prepare_ssr_bridge(&root);
    oj_server::boot_phase("start_dev begin");

    let built_task = tokio::spawn(
        oj_server::DevServer {
            root: root.clone(),
            port,
            bundle: false,
            host,
            config: None,
            enable_cache: false,
            no_cache: false,
            lazy: false,
        }
        .build_app(),
    );

    let route_tree = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || generate_route_tree(&root, &cache))
    };
    let resolver = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || generate_server_fn_resolver(&root, &cache))
    };
    route_tree.await??;
    oj_server::boot_phase("route tree ready");
    let bundle = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || bundle_client_entry_cached(&root, &cache))
    };
    resolver.await??;
    oj_server::boot_phase("resolver ready");
    let runner = Arc::new(tokio::sync::Mutex::new(
        spawn_start_runner(&root, &cache).await?,
    ));
    oj_server::boot_phase("runner spawned");
    let (reload_tx, _) = broadcast::channel::<()>(16);
    {
        let runner = Arc::clone(&runner);
        let reload_tx = reload_tx.clone();
        tokio::spawn(async move {
            let _ = forward(&runner, "GET".into(), "/".into(), vec![], None).await;
            oj_server::boot_phase("prewarm complete");
            if revalidate_runner(&runner).await {
                let _ = reload_tx.send(());
            }
            oj_server::boot_phase("revalidate complete");
        });
    }
    let (bundle_res, built_res) = tokio::join!(bundle, built_task);
    let pinned = bundle_res??;
    let built = built_res??;
    oj_server::boot_phase("bundle+build joined");
    let css_host = if app_uses_tailwind(&root) {
        spawn_node_service(&root, &cache.join("css-host.mjs"))
            .await
            .ok()
            .map(|r| Arc::new(tokio::sync::Mutex::new(r)))
    } else {
        None
    };
    let workerd = spawn_workerd_if_cloudflare(&root, &cache).await;
    let worker_url = workerd.as_ref().map(|w| w.session.worker_url());
    let state = Arc::new(StartState {
        proxy_prefixes: built.proxy_prefixes.clone(),
        plugin_mw_port: built.plugin_mw_port,
        runner,
        bundle: std::sync::RwLock::new(Arc::new(pinned)),
        verify: oj_cache::integrity::VerifyMode::from_env(),
        live_reload: cache.join("live-reload.js"),
        workspace_root: workspace_root(&root),
        reload_tx: reload_tx.clone(),
        ws_tx: built.reload_tx.clone(),
        css_host,
        worker_url,
    });

    spawn_start_watcher(root.clone(), cache.clone(), Arc::clone(&state));
    {
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            start_bundle_store(&root).prune(oj_cache::start_bundle::DEFAULT_PRUNE_BUDGET_BYTES);
        });
    }

    let app = built.router.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&state),
        start_route,
    ));

    let (listener, port) =
        oj_server::bind_dev_listener(built.host, built.port, built.strict_port).await?;
    oj_server::boot_phase("listening");
    {
        let root = root.clone();
        tokio::spawn(async move {
            let code = shutdown_signal().await;
            oj_server::plugins::cleanup_ssr_bridge(&root);
            std::process::exit(code);
        });
    }
    println!("  {} dev (tanstack start)", oj_server::oj_brand());
    let url = format!("http://localhost:{}/", port);
    println!("  {}", oj_server::link(&url, &oj_server::cell(&url)));
    axum::serve(listener, app).await?;
    Ok(())
}

async fn shutdown_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return tokio::signal::ctrl_c().await.map(|_| 130).unwrap_or(130),
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => 130,
            _ = term.recv() => 143,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        130
    }
}

async fn revalidate_runner(runner: &Arc<tokio::sync::Mutex<Runner>>) -> bool {
    let mut guard = runner.lock().await;
    if guard
        .stdin
        .write_all(b"{\"cmd\":\"revalidate\"}\n")
        .await
        .is_err()
        || guard.stdin.flush().await.is_err()
    {
        return false;
    }
    let Ok(Some(line)) = guard.lines.next_line().await else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("reloaded").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

async fn reload_runner(state: &StartState) {
    let mut guard = state.runner.lock().await;
    if guard
        .stdin
        .write_all(b"{\"cmd\":\"reload\"}\n")
        .await
        .is_err()
    {
        return;
    }
    let _ = guard.stdin.flush().await;
    let _ = guard.lines.next_line().await;
}

fn list_route_files(root: &Path) -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    fn walk(dir: &Path, out: &mut std::collections::BTreeSet<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "ts" || x == "tsx") {
                out.insert(p);
            }
        }
    }
    walk(&root.join("src").join("routes"), &mut out);
    out
}

fn workspace_root(app: &Path) -> PathBuf {
    let mut best = app.to_path_buf();
    let mut cur = app;
    while let Some(parent) = cur.parent() {
        if parent.join("node_modules").is_dir() {
            best = parent.to_path_buf();
        }
        cur = parent;
    }
    best
}

fn asset_mime(ext: &str) -> &'static str {
    match ext {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

const RELOAD_CLIENT: &str = "<script type=\"module\" src=\"/@oj-start/live-reload.js\"></script>";

fn spawn_start_watcher(root: PathBuf, cache: PathBuf, state: Arc<StartState>) {
    let rt = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        use std::sync::mpsc::RecvTimeoutError;

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("oj start: file watcher failed: {e}");
                return;
            }
        };
        let src = root.join("src");
        if let Err(e) = watcher.watch(&src, RecursiveMode::Recursive) {
            eprintln!("oj start: cannot watch {}: {e}", src.display());
            return;
        }
        let mut prev_routes = list_route_files(&root);
        let mut batch = 0u64;
        loop {
            let mut paths: std::collections::HashSet<PathBuf> = match rx.recv() {
                Ok(Ok(ev)) => ev.paths.into_iter().collect(),
                Ok(Err(_)) => continue,
                Err(_) => break,
            };
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(Ok(ev)) => paths.extend(ev.paths),
                    Ok(Err(_)) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            // Rebuild only on a real source-file change. Ignore the generated
            // route tree and its atomic-write temp siblings, plus bare directory
            // events (Linux inotify emits a parent-dir event alongside the file
            // write, which would otherwise slip past a filename-only filter and
            // retrigger the generator on every reload).
            let relevant = paths.iter().any(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !name.contains("routeTree.gen") && !p.is_dir()
            });
            if !relevant {
                continue;
            }
            let routes_now = list_route_files(&root);
            let routes_changed = routes_now != prev_routes;
            if routes_changed {
                let _ = generate_route_tree(&root, &cache);
                prev_routes = routes_now;
            }
            let server_fn_changed = paths.iter().any(|p| {
                let is_ts = p.extension().is_some_and(|e| e == "ts" || e == "tsx");
                is_ts
                    && (!p.exists()
                        || std::fs::read_to_string(p).is_ok_and(|s| s.contains("createServerFn")))
            });
            if routes_changed || server_fn_changed {
                let _ = generate_server_fn_resolver(&root, &cache);
            }
            // Narrate the compile batch to the editor: "Applying changes…" while
            // it rebuilds, then done so the pill clears.
            batch += 1;
            let _ = state.ws_tx.send(oj_server::update_progress_frame(
                batch, "watch", 0, 0, None, false,
            ));
            rt.block_on(async {
                let (r, c) = (root.clone(), cache.clone());
                let client = tokio::task::spawn_blocking(move || {
                    if bundle_client_entry(&r, &c).is_err() {
                        return None;
                    }
                    match start_bundle_store(&r).persist(&c) {
                        Some((_, pinned)) => Some(pinned),
                        None => oj_cache::start_bundle::PinnedBundle::from_build_dir(&c),
                    }
                });
                let (pinned, _) = tokio::join!(client, reload_runner(&state));
                if let Ok(Some(pinned)) = pinned {
                    *state.bundle.write().unwrap() = Arc::new(pinned);
                }
            });
            let _ = state.reload_tx.send(());
            let modules = client_module_count(&cache);
            let _ = state.ws_tx.send(oj_server::update_progress_frame(
                batch,
                "watch",
                0,
                modules,
                Some(0),
                true,
            ));
            println!("  oj start: rebuilt, reloading");
        }
    });
}

pub async fn start_build(root: PathBuf) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
    generate_route_tree(&root, &cache)?;
    generate_server_fn_resolver(&root, &cache)?;
    let prerender = oj_config::load(&root)
        .ok()
        .and_then(|c| c.build)
        .and_then(|b| b.prerender)
        .unwrap_or_default()
        .join(",");
    let status = std::process::Command::new("node")
        .arg(cache.join("build.mjs"))
        .env("OJ_APP_ROOT", &root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(&root))
        .env("NODE_ENV", "production")
        .env("OJ_PRERENDER", &prerender)
        .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(&root))
        .current_dir(&root)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run production build (node): {e}"))?;
    if !status.success() {
        anyhow::bail!("production build failed");
    }
    println!(
        "  {} build (tanstack start) -> {}/dist",
        oj_server::oj_brand(),
        root.display()
    );
    println!("  run: node dist/server.mjs");
    Ok(())
}

fn codegen_store(
    root: &Path,
    cache: &Path,
    kind: &str,
    script: &str,
    marker: Option<&str>,
) -> oj_cache::start_codegen::StartCodegenStore {
    let mut extra = std::fs::read(cache.join(script)).unwrap_or_default();
    for name in [
        "tsr.config.json",
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
    ] {
        if let Ok(bytes) = std::fs::read(root.join(name)) {
            extra.extend_from_slice(name.as_bytes());
            extra.push(0);
            extra.extend_from_slice(&bytes);
            extra.push(0);
        }
    }
    oj_cache::start_codegen::StartCodegenStore::new(
        root,
        kind,
        env!("CARGO_PKG_VERSION"),
        &extra,
        marker,
    )
}

fn generate_route_tree(root: &Path, cache: &Path) -> anyhow::Result<()> {
    let store = codegen_store(root, cache, "route-tree", "generate.mjs", None);
    let dest = root.join("src").join("routeTree.gen.ts");
    let outputs = [("routeTree.gen.ts", dest.as_path())];
    let inputs: Vec<PathBuf> = list_route_files(root).into_iter().collect();
    match store.restore(&inputs, &outputs) {
        Ok(stats) => {
            println!(
                "  oj start: route tree restored from cache (key {}…, {} route file(s) verified in {}ms)",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.keyed_files,
                stats.elapsed_ms
            );
            return Ok(());
        }
        Err(miss) => println!("  oj start: route tree cache miss ({miss})"),
    }
    run_node(root, &cache.join("generate.mjs"), "route tree generation")?;
    let inputs: Vec<PathBuf> = list_route_files(root).into_iter().collect();
    store.persist(&inputs, &outputs);
    Ok(())
}

fn generate_server_fn_resolver(root: &Path, cache: &Path) -> anyhow::Result<()> {
    let store = codegen_store(
        root,
        cache,
        "server-fn",
        "gen-resolver.mjs",
        Some("createServerFn"),
    );
    let dest = cache.join("server-fn-resolver.mjs");
    let outputs = [("server-fn-resolver.mjs", dest.as_path())];
    let inputs = list_src_ts_files(root);
    match store.restore(&inputs, &outputs) {
        Ok(stats) => {
            println!(
                "  oj start: server-fn resolver restored from cache (key {}…, {} server-fn file(s), {} of {} rehashed, {}ms)",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.keyed_files,
                stats.rehashed,
                inputs.len(),
                stats.elapsed_ms
            );
            return Ok(());
        }
        Err(miss) => println!("  oj start: server-fn resolver cache miss ({miss})"),
    }
    run_node(root, &cache.join("gen-resolver.mjs"), "server-fn resolver")?;
    store.persist(&inputs, &outputs);
    Ok(())
}

fn list_src_ts_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "ts" || x == "tsx") {
                out.push(p);
            }
        }
    }
    walk(&root.join("src"), &mut out);
    out
}

fn bundle_client_entry(root: &Path, cache: &Path) -> anyhow::Result<()> {
    run_node(
        root,
        &cache.join("bundle-client.mjs"),
        "client entry bundling",
    )
}

fn start_bundle_store(root: &Path) -> oj_cache::start_bundle::StartBundleStore {
    oj_cache::start_bundle::StartBundleStore::new(
        root,
        env!("CARGO_PKG_VERSION"),
        oj_cache::integrity::VerifyMode::from_env(),
    )
}

fn bundle_client_entry_cached(
    root: &Path,
    cache: &Path,
) -> anyhow::Result<oj_cache::start_bundle::PinnedBundle> {
    let store = start_bundle_store(root);
    match store.restore(cache) {
        Ok((stats, pinned)) => {
            let rehashed = match stats.rehashed {
                0 => String::new(),
                n => format!(", {n} re-hashed"),
            };
            println!(
                "  oj start: client bundle restored from cache (key {}…, {} chunks, {} files verified in {}ms{rehashed})",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.chunks,
                stats.files,
                stats.elapsed_ms
            );
            return Ok(pinned);
        }
        Err(miss) => println!("  oj start: client bundle cache miss ({miss})"),
    }
    bundle_client_entry(root, cache)?;
    if let Some((key, pinned)) = store.persist(cache) {
        println!(
            "  oj start: client bundle cached (key {}…, {} chunks)",
            key.get(..8).unwrap_or(&key),
            pinned.len()
        );
        return Ok(pinned);
    }
    oj_cache::start_bundle::PinnedBundle::from_build_dir(cache)
        .ok_or_else(|| anyhow::anyhow!("client chunk index missing after successful bundle"))
}

// Client module count written by bundle-client.mjs, for update/boot narration.
fn client_module_count(cache: &Path) -> usize {
    std::fs::read_to_string(cache.join("client-entry.modules"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn run_node(root: &Path, script: &Path, what: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("node")
        .arg(script)
        .env("OJ_APP_ROOT", root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_ENV", "development")
        .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(root))
        .current_dir(root)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {what} (node): {e}"))?;
    if !status.success() {
        anyhow::bail!("{what} failed");
    }
    Ok(())
}

async fn spawn_start_runner(root: &Path, cache: &Path) -> anyhow::Result<Runner> {
    spawn_node_service(root, &cache.join("runner.mjs")).await
}

struct WorkerdGuard {
    session: oj_server::workerd_dev::WorkerdSession,
    _loader: tokio::process::Child,
}

async fn spawn_workerd_if_cloudflare(root: &Path, cache: &Path) -> Option<WorkerdGuard> {
    if !oj_server::workerd::is_cloudflare_app(root) {
        return None;
    }
    let Some(bin) = oj_server::workerd::find_workerd(root) else {
        eprintln!("oj: cloudflare app detected but no workerd binary found; using node ssr");
        return None;
    };
    let (loader_child, loader_url) = match spawn_plugin_loader(root, cache).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("oj: workerd plugin-loader failed to start ({e}); using node ssr");
            return None;
        }
    };
    let aliases = oj_server::workerd_dev::start_aliases(root, cache);
    let cfg = oj_server::wrangler::load(root);
    let entry = cache.join("server-entry.tsx");
    let opts = oj_server::workerd_dev::WorkerdSpawn::from_wrangler(
        cfg,
        entry.to_string_lossy().into_owned(),
    );
    match oj_server::workerd_dev::spawn(root, &bin, cache, aliases, Some(loader_url), opts).await {
        Ok(session) => {
            eprintln!("oj: cloudflare dev via native workerd at {}", session.worker_url());
            Some(WorkerdGuard { session, _loader: loader_child })
        }
        Err(e) => {
            eprintln!("oj: workerd failed to start ({e}); using node ssr");
            None
        }
    }
}

async fn spawn_plugin_loader(
    root: &Path,
    cache: &Path,
) -> anyhow::Result<(tokio::process::Child, String)> {
    let mut cmd = tokio::process::Command::new("node");
    cmd.arg(cache.join("workerd-plugin-loader.mjs"))
        .arg("0")
        .env("OJ_APP_ROOT", root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_ENV", "development")
        .env("OJ_SSR_BRIDGE_DIR", oj_server::plugins::ssr_bridge_dir(root))
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let port = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(p) = line.strip_prefix("OJ_LOADER_PORT=") {
                return p.trim().parse::<u16>().ok();
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    match port {
        Some(p) => Ok((child, format!("http://127.0.0.1:{p}/"))),
        None => anyhow::bail!("plugin loader did not report a port"),
    }
}

async fn spawn_node_service(root: &Path, script: &Path) -> anyhow::Result<Runner> {
    let mut cmd = tokio::process::Command::new("node");
    cmd.arg(script)
        .env("OJ_APP_ROOT", root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_ENV", "development")
        .env(
            "OJ_SSR_BRIDGE_DIR",
            oj_server::plugins::ssr_bridge_dir(root),
        )
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(v8) = oj_server::node_compile_cache_opt_in(root) {
        cmd.env("NODE_COMPILE_CACHE", v8);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn node service {}: {e}", script.display()))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(Runner {
        stdin,
        lines: BufReader::new(stdout).lines(),
        _child: child,
    })
}

fn app_uses_tailwind(root: &Path) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .map(|s| {
            s.contains("@tailwindcss/postcss")
                || s.contains("@tailwindcss/vite")
                || s.contains("\"tailwindcss\"")
        })
        .unwrap_or(false)
}

fn needs_css_compile(src: &str) -> bool {
    src.contains("tailwindcss")
        || src.contains("@tailwind")
        || src.contains("@plugin")
        || src.contains("@apply")
}

async fn compile_css(host: &Arc<tokio::sync::Mutex<Runner>>, path: &Path) -> Option<String> {
    let mut guard = host.lock().await;
    let req = serde_json::json!({ "path": path.to_string_lossy() });
    guard
        .stdin
        .write_all(format!("{req}\n").as_bytes())
        .await
        .ok()?;
    guard.stdin.flush().await.ok()?;
    let line = tokio::time::timeout(std::time::Duration::from_secs(30), guard.lines.next_line())
        .await
        .ok()?
        .ok()??;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.get("css").and_then(|c| c.as_str()).map(|s| s.to_owned())
}

enum Route {
    Document,
    Pass,
}

fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    let path = req.uri().path();
    if path.starts_with("/@") || path.starts_with("/__") {
        return Route::Pass;
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if last != "index.html" && last.contains('.') {
        return Route::Pass;
    }
    if proxy_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
        return Route::Pass;
    }
    match *req.method() {
        Method::GET => Route::Document,
        _ => Route::Pass,
    }
}

fn document_url(url: &str) -> String {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, format!("?{q}")),
        None => (url, String::new()),
    };
    let path = match path.strip_suffix("/index.html") {
        Some(prefix) => format!("{prefix}/"),
        None => path.to_string(),
    };
    format!("{path}{query}")
}

async fn start_route(State(state): State<Arc<StartState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/@oj-start/hmr" {
        let (mut parts, _) = req.into_parts();
        return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(ws) => {
                let mut rx = state.reload_tx.subscribe();
                ws.on_upgrade(move |mut socket| async move {
                    while rx.recv().await.is_ok() {
                        if socket.send(Message::Text("reload".into())).await.is_err() {
                            break;
                        }
                    }
                })
            }
            Err(e) => e.into_response(),
        };
    }
    if req.uri().path() == "/@oj-start/live-reload.js" {
        return serve_js(&state.live_reload, "live-reload client").await;
    }
    if let Some(rest) = req.uri().path().strip_prefix("/@oj-start/fs/") {
        return serve_fs_asset(&state, &format!("/{rest}")).await;
    }
    if let Some(name) = req.uri().path().strip_prefix("/@oj-start/") {
        return serve_client_chunk(&state, name).await;
    }
    if req.uri().path().starts_with("/_serverFn/") {
        let method = req.method().to_string();
        let url = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_string();
        let headers = collect_headers(req.headers());
        let body = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
            .await
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        return forward(&state.runner, method, url, headers, body).await;
    }
    match classify(&req, &state.proxy_prefixes) {
        Route::Document => {
            let raw = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/")
                .to_string();
            // Editor plugins (dev-server bridge) register configureServer routes
            // with no path prefix, so a GET like /_sandbox/preview/viewers can only
            // be told from an app route by asking the middleware first; it returns
            // x-oj-fallthrough when it does not own the path, and then we SSR.
            if let Some(port) = state.plugin_mw_port {
                if let Some(resp) =
                    oj_server::forward_get_to_plugin_mw(port, &raw, req.headers()).await
                {
                    return resp;
                }
            }
            if let Some(worker_url) = &state.worker_url {
                if let Some(resp) =
                    oj_server::forward_get_to_worker(worker_url, &raw, req.headers()).await
                {
                    return resp;
                }
            }
            forward(&state.runner, "GET".into(), document_url(&raw), vec![], None).await
        }
        Route::Pass => next.run(req).await,
    }
}

async fn serve_client_chunk(state: &StartState, name: &str) -> Response {
    let name = percent_decode(name);
    let bundle = state
        .bundle
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(chunk) = bundle.chunk(&name) else {
        return (
            StatusCode::NOT_FOUND,
            format!("oj start: {name} is not in the client bundle manifest"),
        )
            .into_response();
    };
    let mode = match &chunk.hash {
        Some(_) => state.verify,
        None => oj_cache::integrity::VerifyMode::Standard,
    };
    let expected = oj_cache::integrity::ExpectedFile {
        size: chunk.size,
        hash: chunk.hash.clone().unwrap_or_default(),
    };
    let path = chunk.path.clone();
    let read = tokio::task::spawn_blocking(move || {
        oj_cache::integrity::verified_read(&path, &expected, mode)
    })
    .await;
    match read {
        Ok(Ok(bytes)) => {
            let ext = name.rsplit('.').next().unwrap_or("");
            (
                [
                    (header::CONTENT_TYPE, asset_mime(ext)),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => {
            eprintln!("oj start: chunk {name} failed verification ({e}); refusing to serve");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("oj start: chunk {name}: {e}"),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "oj start: chunk read task failed".to_string(),
        )
            .into_response(),
    }
}

async fn serve_js(path: &Path, what: &str) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj start: {what}: {e}"),
        )
            .into_response(),
    }
}

async fn serve_fs_asset(state: &StartState, abs: &str) -> Response {
    let decoded = percent_decode(abs);
    let path = PathBuf::from(&decoded);
    let canon = match tokio::fs::canonicalize(&path).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::NOT_FOUND, format!("oj start: asset: {e}")).into_response(),
    };
    if !canon.starts_with(&state.workspace_root) {
        return (StatusCode::FORBIDDEN, "oj start: asset outside workspace").into_response();
    }
    if canon.extension().and_then(|e| e.to_str()) == Some("css") {
        if let Some(host) = &state.css_host {
            if let Ok(src) = tokio::fs::read_to_string(&canon).await {
                if needs_css_compile(&src) {
                    if let Some(css) = compile_css(host, &canon).await {
                        return (
                            [
                                (header::CONTENT_TYPE, "text/css; charset=utf-8"),
                                (header::CACHE_CONTROL, "no-cache"),
                            ],
                            css,
                        )
                            .into_response();
                    }
                }
            }
        }
    }
    match tokio::fs::read(&canon).await {
        Ok(bytes) => {
            let ext = canon.extension().and_then(|e| e.to_str()).unwrap_or("");
            (
                [
                    (header::CONTENT_TYPE, asset_mime(ext)),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj start: asset: {e}"),
        )
            .into_response(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn collect_headers(headers: &header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect()
}

async fn forward(
    runner: &Arc<tokio::sync::Mutex<Runner>>,
    method: String,
    url: String,
    req_headers: Vec<(String, String)>,
    body: Option<String>,
) -> Response {
    let runner = Arc::clone(runner);
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut guard = runner.lock_owned().await;
        let result = async {
            let hdrs: serde_json::Map<String, serde_json::Value> = req_headers
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            let cmd =
                serde_json::json!({ "method": method, "url": url, "headers": hdrs, "body": body });
            guard
                .stdin
                .write_all(format!("{cmd}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            guard.stdin.flush().await.map_err(|e| e.to_string())?;
            let line = guard
                .lines
                .next_line()
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "start runner closed".to_string())?;
            serde_json::from_str::<serde_json::Value>(&line).map_err(|e| e.to_string())
        }
        .await;
        let _ = tx.send(result);
    });
    match rx
        .await
        .unwrap_or_else(|_| Err("start runner task cancelled".to_string()))
    {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
            let mut body = v
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or("")
                .to_owned();
            let is_html = v
                .get("headers")
                .and_then(|h| h.get("content-type"))
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains("text/html"));
            if is_html {
                if let Some(i) = body.rfind("</body>") {
                    body.insert_str(i, RELOAD_CLIENT);
                } else {
                    body.push_str(RELOAD_CLIENT);
                }
            }
            let mut resp = Response::new(axum::body::Body::from(body));
            *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            if let Some(h) = v.get("headers").and_then(|h| h.as_object()) {
                for (k, val) in h {
                    let lower = k.to_ascii_lowercase();
                    if lower == "content-length"
                        || lower == "content-encoding"
                        || lower == "transfer-encoding"
                    {
                        continue;
                    }
                    if let (Ok(name), Some(vs)) =
                        (header::HeaderName::from_bytes(k.as_bytes()), val.as_str())
                    {
                        if let Ok(value) = header::HeaderValue::from_str(vs) {
                            resp.headers_mut().insert(name, value);
                        }
                    }
                }
            }
            resp
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oj-startdev-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(method: &str, path: &str) -> Request {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[test]
    fn app_uses_tailwind_detects_postcss_vite_and_bare() {
        let base = tmp("tw");
        let cases = [
            (r#"{"devDependencies":{"@tailwindcss/postcss":"4"}}"#, true),
            (r#"{"devDependencies":{"@tailwindcss/vite":"4"}}"#, true),
            (r#"{"dependencies":{"tailwindcss":"3"}}"#, true),
            (r#"{"dependencies":{"react":"19"}}"#, false),
        ];
        for (i, (pkg, expected)) in cases.iter().enumerate() {
            let app = base.join(format!("app{i}"));
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(app.join("package.json"), pkg).unwrap();
            assert_eq!(app_uses_tailwind(&app), *expected, "case {i}: {pkg}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn document_url_rewrites_index_html_to_directory() {
        assert_eq!(document_url("/"), "/");
        assert_eq!(document_url("/index.html"), "/");
        assert_eq!(document_url("/guides/index.html"), "/guides/");
        assert_eq!(document_url("/a/b/index.html"), "/a/b/");
        assert_eq!(document_url("/index.html?tab=1"), "/?tab=1");
        assert_eq!(document_url("/a/index.html?x=y&z=1"), "/a/?x=y&z=1");
        assert_eq!(document_url("/about"), "/about");
        assert_eq!(document_url("/guides/foo"), "/guides/foo");
        assert_eq!(document_url("/notindex.html"), "/notindex.html");
    }

    #[test]
    fn percent_decode_handles_escapes_and_partials() {
        assert_eq!(percent_decode("/plain/path.css"), "/plain/path.css");
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("%2Fx"), "/x");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn asset_mime_maps_types() {
        assert_eq!(asset_mime("css"), "text/css; charset=utf-8");
        assert_eq!(asset_mime("js"), "text/javascript");
        assert_eq!(asset_mime("svg"), "image/svg+xml");
        assert_eq!(asset_mime("webp"), "image/webp");
        assert_eq!(asset_mime("woff2"), "font/woff2");
        assert_eq!(asset_mime("json"), "application/json");
        assert_eq!(asset_mime("weirdext"), "application/octet-stream");
    }

    #[test]
    fn needs_css_compile_detects_tailwind_markers() {
        assert!(needs_css_compile("@import \"tailwindcss\";"));
        assert!(needs_css_compile("@tailwind base;"));
        assert!(needs_css_compile("@plugin \"@tailwindcss/typography\";"));
        assert!(needs_css_compile(".btn { @apply px-2; }"));
        assert!(!needs_css_compile(".a { color: red }"));
        assert!(!needs_css_compile("@font-face { src: url(x.woff2) }"));
    }

    #[test]
    fn workspace_root_finds_farthest_node_modules() {
        let base = tmp("ws");
        std::fs::create_dir_all(base.join("node_modules")).unwrap();
        std::fs::create_dir_all(base.join("web").join("src")).unwrap();
        std::fs::create_dir_all(base.join("web").join("node_modules")).unwrap();
        assert_eq!(workspace_root(&base.join("web")), base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_root_defaults_to_app_without_ancestor_modules() {
        let base = tmp("ws2");
        let app = base.join("solo");
        std::fs::create_dir_all(&app).unwrap();
        assert_eq!(workspace_root(&app), app);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn app_uses_tailwind_reads_package_json() {
        let base = tmp("tw-pkg");
        std::fs::write(
            base.join("package.json"),
            r#"{"devDependencies":{"@tailwindcss/postcss":"4"}}"#,
        )
        .unwrap();
        assert!(app_uses_tailwind(&base));
        std::fs::write(
            base.join("package.json"),
            r#"{"dependencies":{"react":"19"}}"#,
        )
        .unwrap();
        assert!(!app_uses_tailwind(&base));
        let none = tmp("tw-none");
        assert!(!app_uses_tailwind(&none));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&none);
    }

    #[test]
    fn classify_documents_vs_passes() {
        let no_proxy: Vec<String> = vec![];
        assert!(matches!(
            classify(&req("GET", "/"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/about"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/index.html"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/guides/index.html"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/main.js"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("GET", "/styles.css"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("GET", "/@oj-start/hmr"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("GET", "/__health"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("POST", "/about"), &no_proxy),
            Route::Pass
        ));
        let proxy = vec!["/api".to_string()];
        assert!(matches!(
            classify(&req("GET", "/api/users"), &proxy),
            Route::Pass
        ));
    }

    #[test]
    fn list_route_files_collects_ts_tsx_recursively() {
        let root = tmp("routes");
        let routes = root.join("src").join("routes");
        std::fs::create_dir_all(routes.join("nested")).unwrap();
        std::fs::write(routes.join("index.tsx"), "").unwrap();
        std::fs::write(routes.join("about.ts"), "").unwrap();
        std::fs::write(routes.join("styles.css"), "").unwrap();
        std::fs::write(routes.join("data.json"), "").unwrap();
        std::fs::write(routes.join("nested").join("deep.tsx"), "").unwrap();
        let found = list_route_files(&root);
        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(found.len(), 3, "only ts/tsx counted: {names:?}");
        assert!(names.contains(&"index.tsx".to_string()));
        assert!(names.contains(&"about.ts".to_string()));
        assert!(
            names.contains(&"deep.tsx".to_string()),
            "recursion into subdirs: {names:?}"
        );
        assert!(!names
            .iter()
            .any(|n| n.ends_with(".css") || n.ends_with(".json")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_src_ts_files_walks_all_of_src() {
        let root = tmp("srcwalk");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("routes")).unwrap();
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("routes").join("index.tsx"), "").unwrap();
        std::fs::write(src.join("lib").join("fns.ts"), "").unwrap();
        std::fs::write(src.join("lib").join("types.d.ts"), "").unwrap();
        std::fs::write(src.join("styles.css"), "").unwrap();
        let found = list_src_ts_files(&root);
        assert_eq!(found.len(), 3, "ts/tsx (incl. .d.ts) only: {found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_route_files_missing_dir_is_empty() {
        let root = tmp("noroutes");
        assert!(list_route_files(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_headers_serializes_utf8_and_skips_non_utf8() {
        let mut h = header::HeaderMap::new();
        h.insert(
            "content-type",
            header::HeaderValue::from_static("text/html"),
        );
        h.insert("x-custom", header::HeaderValue::from_static("hello"));
        h.insert(
            "x-bin",
            header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        let out = collect_headers(&h);
        assert!(
            out.contains(&("content-type".to_string(), "text/html".to_string())),
            "{out:?}"
        );
        assert!(
            out.contains(&("x-custom".to_string(), "hello".to_string())),
            "{out:?}"
        );
        assert!(
            !out.iter().any(|(k, _)| k == "x-bin"),
            "non-utf8 value dropped: {out:?}"
        );
    }
}
