// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev-server support for TanStack Start apps (`oj dev`, auto-detected).
//!
//! oj generates the route tree with `@tanstack/router-generator`, synthesizes
//! the server/client/manifest entries the TanStack Vite plugin would, and runs
//! the server entry in a persistent Node process behind a loader hook that
//! resolves the framework's four alias specifiers. Document requests are
//! answered by the entry's `fetch(request)`; modules and assets fall through to
//! the normal dev pipeline.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, Request, State, WebSocketUpgrade, ws::Message},
    http::{Method, StatusCode, header},
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
    runner: Arc<tokio::sync::Mutex<Runner>>,
    /// The browser-bundled client hydration entry, served at
    /// `/@oj-start/client-entry.js`.
    client_bundle: PathBuf,
    /// The live-reload client (snapshot/restore + reconnecting WebSocket),
    /// served at `/@oj-start/live-reload.js`.
    live_reload: PathBuf,
    /// Trust boundary for `/@oj-start/fs/` asset serving: the farthest ancestor
    /// of the app root that has a `node_modules` (the pnpm/workspace root), so
    /// assets under sibling workspace packages and the shared store resolve.
    workspace_root: PathBuf,
    /// Fires after a rebuild so the injected client reloads the page.
    reload_tx: broadcast::Sender<()>,
    /// Persistent CSS compiler (PostCSS + Tailwind v4). Absent when the app has
    /// no `@tailwindcss/postcss`; then css is served raw.
    css_host: Option<Arc<tokio::sync::Mutex<Runner>>>,
}

pub async fn start_dev(root: PathBuf, port: Option<u16>) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = root.join(".oj-cache").join("start");
    oj_server::write_start_assets(&cache)?;

    // Cold start, parallelized. These are otherwise-idle `node` spawns that used
    // to run strictly back to back. Only two real dependencies constrain them:
    // the client bundle esbuild's the router, which imports `routeTree.gen.ts`,
    // and the runner imports the server entry (route tree + server-fn resolver)
    // and the bundle's manifest. Everything else overlaps.
    //
    //   route-tree gen ─┬─────────────────┐
    //   resolver gen ───┼──(both done)────┴─> client bundle ─┐
    //                   └──> build_app (Rust) ───────────────┴─> runner
    //
    // Route-tree and resolver generation are independent node processes: launch
    // both at once instead of one, then the other.
    let route_tree = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || generate_route_tree(&root, &cache))
    };
    let resolver = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || {
            run_node(&root, &cache.join("gen-resolver.mjs"), "server-fn resolver")
        })
    };
    // The client bundle needs the route tree; the Rust dev-server build overlaps
    // it (build_app crawls the same source but writes nothing the bundle reads).
    route_tree.await??;
    let bundle = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || bundle_client_entry(&root, &cache))
    };
    // Reuse the dev server for module/asset serving; the document route sits on top.
    let built_fut = oj_server::DevServer { root: root.clone(), port, bundle: false }.build_app();
    let (bundle_res, built_res) = tokio::join!(bundle, built_fut);
    bundle_res??;
    resolver.await??;
    let built = built_res?;
    // Runner needs the route tree, resolver, and the bundle's manifest — all done.
    let runner = spawn_start_runner(&root, &cache).await?;
    // Only spawn the CSS compiler when the app actually uses Tailwind's PostCSS
    // plugin; otherwise css is served as-is.
    let css_host = if app_uses_tailwind(&root) {
        spawn_node_service(&root, &cache.join("css-host.mjs"))
            .await
            .ok()
            .map(|r| Arc::new(tokio::sync::Mutex::new(r)))
    } else {
        None
    };
    let (reload_tx, _) = broadcast::channel::<()>(16);
    let state = Arc::new(StartState {
        proxy_prefixes: built.proxy_prefixes.clone(),
        runner: Arc::new(tokio::sync::Mutex::new(runner)),
        client_bundle: cache.join("client-entry.js"),
        live_reload: cache.join("live-reload.js"),
        workspace_root: workspace_root(&root),
        reload_tx: reload_tx.clone(),
        css_host,
    });

    // Watch src/: on change, regenerate the route tree, rebuild the client, and
    // restart the runner (so SSR is fresh), then reload the page.
    spawn_start_watcher(root.clone(), cache.clone(), Arc::clone(&state));

    let app = built
        .router
        .layer(axum::middleware::from_fn_with_state(Arc::clone(&state), start_route));

    let addr = std::net::SocketAddr::from((built.host, built.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
    println!("  {} dev (tanstack start)", oj_server::oj_brand());
    let url = format!("http://localhost:{}/", built.port);
    println!("  {}", oj_server::link(&url, &oj_server::cobalt(&url)));
    axum::serve(listener, app).await?;
    Ok(())
}

/// Tell the warm runner to re-import the server entry in place, and drain its
/// ack line so the request/response protocol stays aligned.
async fn reload_runner(state: &StartState) {
    let mut guard = state.runner.lock().await;
    if guard.stdin.write_all(b"{\"cmd\":\"reload\"}\n").await.is_err() {
        return;
    }
    let _ = guard.stdin.flush().await;
    let _ = guard.lines.next_line().await;
}

/// The set of route source files under `src/routes` (recursive). Used to
/// decide whether the route tree needs regenerating.
fn list_route_files(root: &Path) -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    fn walk(dir: &Path, out: &mut std::collections::BTreeSet<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
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

/// The farthest ancestor of `app` (including itself) that contains a
/// `node_modules`. Under pnpm workspaces this is the repo root, so assets in
/// sibling packages and the shared store are reachable via `/@oj-start/fs/`.
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

/// Injected into HTML documents in dev: loads the live-reload client, which
/// snapshots ephemeral state, reloads on a rebuild signal, then restores it.
const RELOAD_CLIENT: &str = "<script type=\"module\" src=\"/@oj-start/live-reload.js\"></script>";

/// Watch `src/`: on a real change, regenerate the route tree + resolver,
/// rebuild the client bundle, restart the runner (fresh SSR), then reload.
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
        // Regenerate the route tree only when the route-file SET changes (add/
        // remove/rename); content edits leave the tree unchanged, so skip the
        // generator (its cost scales with route count).
        let mut prev_routes = list_route_files(&root);
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
            // Ignore our own generated route tree to avoid a rebuild loop.
            if !paths.iter().any(|p| !p.ends_with("routeTree.gen.ts")) {
                continue;
            }
            let routes_now = list_route_files(&root);
            let routes_changed = routes_now != prev_routes;
            if routes_changed {
                let _ = generate_route_tree(&root, &cache);
                prev_routes = routes_now;
            }
            // Regenerate the server-fn resolver only when a server-fn file was
            // added/removed/edited (or routes changed), not on every edit.
            let server_fn_changed = paths.iter().any(|p| {
                let is_ts = p.extension().is_some_and(|e| e == "ts" || e == "tsx");
                is_ts && (!p.exists() || std::fs::read_to_string(p).is_ok_and(|s| s.contains("createServerFn")))
            });
            if routes_changed || server_fn_changed {
                let _ = run_node(&root, &cache.join("gen-resolver.mjs"), "server-fn resolver");
            }
            // Rebuild the client and warm-reload the runner concurrently. The
            // runner re-imports the entry in place (app + @tanstack re-evaluate,
            // React stays warm) rather than respawning the process.
            rt.block_on(async {
                let (r, c) = (root.clone(), cache.clone());
                let client = tokio::task::spawn_blocking(move || {
                    let _ = bundle_client_entry(&r, &c);
                });
                let (_, _) = tokio::join!(client, reload_runner(&state));
            });
            let _ = state.reload_tx.send(());
            println!("  oj start: rebuilt, reloading");
        }
    });
}

/// Production build for a TanStack Start app (`oj build`): generate the route
/// tree + resolver, then run the esbuild pipeline that writes `dist/`.
pub async fn start_build(root: PathBuf) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = root.join(".oj-cache").join("start");
    oj_server::write_start_assets(&cache)?;
    generate_route_tree(&root, &cache)?;
    run_node(&root, &cache.join("gen-resolver.mjs"), "server-fn resolver")?;
    // Routes to prerender (SSG) from oj.config `build.prerender`.
    let prerender = oj_config::load(&root)
        .ok()
        .and_then(|c| c.build)
        .and_then(|b| b.prerender)
        .unwrap_or_default()
        .join(",");
    let status = std::process::Command::new("node")
        .arg(cache.join("build.mjs"))
        .env("OJ_APP_ROOT", &root)
        .env("NODE_ENV", "production")
        .env("OJ_PRERENDER", &prerender)
        .current_dir(&root)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run production build (node): {e}"))?;
    if !status.success() {
        anyhow::bail!("production build failed");
    }
    println!("  {} build (tanstack start) -> {}/dist", oj_server::oj_brand(), root.display());
    println!("  run: node dist/server.mjs");
    Ok(())
}

/// Run `@tanstack/router-generator` once to write `src/routeTree.gen.ts`.
fn generate_route_tree(root: &Path, cache: &Path) -> anyhow::Result<()> {
    run_node(root, &cache.join("generate.mjs"), "route tree generation")
}

/// Bundle the client hydration entry for the browser (esbuild, aliases resolved).
fn bundle_client_entry(root: &Path, cache: &Path) -> anyhow::Result<()> {
    run_node(root, &cache.join("bundle-client.mjs"), "client entry bundling")
}

fn run_node(root: &Path, script: &Path, what: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("node")
        .arg(script)
        .env("OJ_APP_ROOT", root)
        .env("NODE_ENV", "development")
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

/// Spawn a persistent Node service (`runner.mjs`, `css-host.mjs`) that speaks
/// JSON lines over stdio. stderr is inherited so its logs are visible.
async fn spawn_node_service(root: &Path, script: &Path) -> anyhow::Result<Runner> {
    let mut child = tokio::process::Command::new("node")
        .arg(script)
        .env("OJ_APP_ROOT", root)
        .env("NODE_ENV", "development")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn node service {}: {e}", script.display()))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(Runner { stdin, lines: BufReader::new(stdout).lines(), _child: child })
}

/// Whether the app depends on `@tailwindcss/postcss` (Tailwind v4 via PostCSS),
/// so css served in dev needs compiling rather than passing through raw.
fn app_uses_tailwind(root: &Path) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .map(|s| s.contains("@tailwindcss/postcss"))
        .unwrap_or(false)
}

/// True when a stylesheet needs Tailwind/PostCSS compilation (v4 `@import
/// "tailwindcss"` or the `@tailwind` / `@plugin` / `@apply` at-rules).
fn needs_css_compile(src: &str) -> bool {
    src.contains("tailwindcss") || src.contains("@tailwind") || src.contains("@plugin") || src.contains("@apply")
}

/// Compile a css file through the CSS host. Returns None on any failure so the
/// caller can fall back to serving the raw file.
async fn compile_css(host: &Arc<tokio::sync::Mutex<Runner>>, path: &Path) -> Option<String> {
    let mut guard = host.lock().await;
    let req = serde_json::json!({ "path": path.to_string_lossy() });
    guard.stdin.write_all(format!("{req}\n").as_bytes()).await.ok()?;
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

/// App document routes are extensionless GETs that aren't a dev prefix (`/@`,
/// `/__`) or a proxy prefix. Everything else falls through to the dev pipeline.
fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    let path = req.uri().path();
    if path.starts_with("/@") || path.starts_with("/__") {
        return Route::Pass;
    }
    // A request for `index.html` is the document for its directory, not a static
    // file (a TanStack Start app has no index.html on disk); everything else
    // with an extension falls through to the dev pipeline.
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

/// Serve `.../index.html` as the document for `.../` so the router matches the
/// directory route instead of a nonexistent file. Preserves the query string.
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
    // Live-reload channel: the injected client reconnects and reloads on a ping.
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
    // The browser-bundled client hydration entry (esbuild, aliases resolved).
    if req.uri().path() == "/@oj-start/client-entry.js" {
        return serve_js(&state.client_bundle, "client entry").await;
    }
    // The live-reload client (snapshot/restore + reconnecting WebSocket).
    if req.uri().path() == "/@oj-start/live-reload.js" {
        return serve_js(&state.live_reload, "live-reload client").await;
    }
    // Asset files referenced by the client bundle (?url imports, side-effect
    // CSS). The path after the prefix is the file's real absolute path, bounded
    // to the workspace root; relative url() refs inside CSS resolve here too.
    if let Some(abs) = req.uri().path().strip_prefix("/@oj-start/fs") {
        return serve_fs_asset(&state, abs).await;
    }
    // Server-function RPC: forward the whole request (method/headers/body) to
    // the runner's fetch handler, which dispatches it via `handleServerAction`.
    if req.uri().path().starts_with("/_serverFn/") {
        let method = req.method().to_string();
        let url = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
        let headers = collect_headers(req.headers());
        let body = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
            .await
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        return forward(&state, method, url, headers, body).await;
    }
    match classify(&req, &state.proxy_prefixes) {
        Route::Document => {
            let raw = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
            forward(&state, "GET".into(), document_url(raw), vec![], None).await
        }
        Route::Pass => next.run(req).await,
    }
}

/// Serve a cached dev JS asset (no-cache so rebuilds always take effect).
async fn serve_js(path: &Path, what: &str) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")],
            bytes,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {what}: {e}")).into_response(),
    }
}

/// Serve an asset file by absolute path, bounded to the workspace root. `abs`
/// is the URL path after `/@oj-start/fs` (starts with `/`), percent-decoded.
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
    // Tailwind/PostCSS stylesheets are compiled by the CSS host before serving;
    // everything else (and any compile failure) is served as-is.
    if canon.extension().and_then(|e| e.to_str()) == Some("css") {
        if let Some(host) = &state.css_host {
            if let Ok(src) = tokio::fs::read_to_string(&canon).await {
                if needs_css_compile(&src) {
                    if let Some(css) = compile_css(host, &canon).await {
                        return (
                            [(header::CONTENT_TYPE, "text/css; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")],
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
                [(header::CONTENT_TYPE, asset_mime(ext)), (header::CACHE_CONTROL, "no-cache")],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: asset: {e}")).into_response(),
    }
}

/// Minimal percent-decode for `%XX` sequences in a URL path.
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

/// Forward a request to the runner's fetch handler and return its response.
/// Detached so a cancelled request still drains the runner's one-line reply.
async fn forward(
    state: &StartState,
    method: String,
    url: String,
    req_headers: Vec<(String, String)>,
    body: Option<String>,
) -> Response {
    let runner = Arc::clone(&state.runner);
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut guard = runner.lock_owned().await;
        let result = async {
            let hdrs: serde_json::Map<String, serde_json::Value> =
                req_headers.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
            let cmd = serde_json::json!({ "method": method, "url": url, "headers": hdrs, "body": body });
            guard.stdin.write_all(format!("{cmd}\n").as_bytes()).await.map_err(|e| e.to_string())?;
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
    match rx.await.unwrap_or_else(|_| Err("start runner task cancelled".to_string())) {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
            let mut body = v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_owned();
            let is_html = v
                .get("headers")
                .and_then(|h| h.get("content-type"))
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains("text/html"));
            // Inject the live-reload client into HTML documents.
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
                    // Skip framing headers: the body is re-sent verbatim.
                    let lower = k.to_ascii_lowercase();
                    if lower == "content-length" || lower == "content-encoding" || lower == "transfer-encoding" {
                        continue;
                    }
                    if let (Ok(name), Some(vs)) = (header::HeaderName::from_bytes(k.as_bytes()), val.as_str()) {
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
    fn document_url_rewrites_index_html_to_directory() {
        assert_eq!(document_url("/"), "/");
        assert_eq!(document_url("/index.html"), "/");
        assert_eq!(document_url("/guides/index.html"), "/guides/");
        assert_eq!(document_url("/a/b/index.html"), "/a/b/");
        assert_eq!(document_url("/index.html?tab=1"), "/?tab=1");
        assert_eq!(document_url("/a/index.html?x=y&z=1"), "/a/?x=y&z=1");
        // non-index documents pass through untouched
        assert_eq!(document_url("/about"), "/about");
        assert_eq!(document_url("/guides/foo"), "/guides/foo");
        // only a whole `/index.html` segment is rewritten
        assert_eq!(document_url("/notindex.html"), "/notindex.html");
    }

    #[test]
    fn percent_decode_handles_escapes_and_partials() {
        assert_eq!(percent_decode("/plain/path.css"), "/plain/path.css");
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("%2Fx"), "/x");
        assert_eq!(percent_decode("%41%42"), "AB");
        // incomplete / trailing escapes are left verbatim
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
        // ancestor `base` has node_modules, so it wins over the app dir
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
        let base = tmp("tw");
        std::fs::write(base.join("package.json"), r#"{"devDependencies":{"@tailwindcss/postcss":"4"}}"#).unwrap();
        assert!(app_uses_tailwind(&base));
        std::fs::write(base.join("package.json"), r#"{"dependencies":{"react":"19"}}"#).unwrap();
        assert!(!app_uses_tailwind(&base));
        let none = tmp("tw-none");
        assert!(!app_uses_tailwind(&none));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&none);
    }

    #[test]
    fn classify_documents_vs_passes() {
        let no_proxy: Vec<String> = vec![];
        // extensionless GETs and index.html are document (SSR) routes
        assert!(matches!(classify(&req("GET", "/"), &no_proxy), Route::Document));
        assert!(matches!(classify(&req("GET", "/about"), &no_proxy), Route::Document));
        assert!(matches!(classify(&req("GET", "/index.html"), &no_proxy), Route::Document));
        assert!(matches!(classify(&req("GET", "/guides/index.html"), &no_proxy), Route::Document));
        // assets, dev namespaces, non-GET, and proxied paths pass through
        assert!(matches!(classify(&req("GET", "/main.js"), &no_proxy), Route::Pass));
        assert!(matches!(classify(&req("GET", "/styles.css"), &no_proxy), Route::Pass));
        assert!(matches!(classify(&req("GET", "/@oj-start/hmr"), &no_proxy), Route::Pass));
        assert!(matches!(classify(&req("GET", "/__health"), &no_proxy), Route::Pass));
        assert!(matches!(classify(&req("POST", "/about"), &no_proxy), Route::Pass));
        let proxy = vec!["/api".to_string()];
        assert!(matches!(classify(&req("GET", "/api/users"), &proxy), Route::Pass));
    }

    #[test]
    fn list_route_files_collects_ts_tsx_recursively() {
        let root = tmp("routes");
        let routes = root.join("src").join("routes");
        std::fs::create_dir_all(routes.join("nested")).unwrap();
        std::fs::write(routes.join("index.tsx"), "").unwrap();
        std::fs::write(routes.join("about.ts"), "").unwrap();
        std::fs::write(routes.join("styles.css"), "").unwrap(); // not a route
        std::fs::write(routes.join("data.json"), "").unwrap(); // not a route
        std::fs::write(routes.join("nested").join("deep.tsx"), "").unwrap();
        let found = list_route_files(&root);
        let names: Vec<String> =
            found.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        assert_eq!(found.len(), 3, "only ts/tsx counted: {names:?}");
        assert!(names.contains(&"index.tsx".to_string()));
        assert!(names.contains(&"about.ts".to_string()));
        assert!(names.contains(&"deep.tsx".to_string()), "recursion into subdirs: {names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".css") || n.ends_with(".json")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_route_files_missing_dir_is_empty() {
        let root = tmp("noroutes"); // exists, but has no src/routes
        assert!(list_route_files(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_headers_serializes_utf8_and_skips_non_utf8() {
        let mut h = header::HeaderMap::new();
        h.insert("content-type", header::HeaderValue::from_static("text/html"));
        h.insert("x-custom", header::HeaderValue::from_static("hello"));
        // an obs-text (non-UTF8) value is dropped rather than panicking
        h.insert("x-bin", header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
        let out = collect_headers(&h);
        assert!(out.contains(&("content-type".to_string(), "text/html".to_string())), "{out:?}");
        assert!(out.contains(&("x-custom".to_string(), "hello".to_string())), "{out:?}");
        assert!(!out.iter().any(|(k, _)| k == "x-bin"), "non-utf8 value dropped: {out:?}");
    }
}
