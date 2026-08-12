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
    /// Fires after a rebuild so the injected client reloads the page.
    reload_tx: broadcast::Sender<()>,
}

pub async fn start_dev(root: PathBuf, port: Option<u16>) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = root.join(".oj-cache").join("start");
    oj_server::write_start_assets(&cache)?;
    generate_route_tree(&root, &cache)?;
    run_node(&root, &cache.join("gen-resolver.mjs"), "server-fn resolver")?;
    bundle_client_entry(&root, &cache)?;

    // Reuse the dev server for module/asset serving; the document route sits on top.
    let built = oj_server::DevServer { root: root.clone(), port, bundle: false }.build_app().await?;
    let runner = spawn_start_runner(&root, &cache).await?;
    let (reload_tx, _) = broadcast::channel::<()>(16);
    let state = Arc::new(StartState {
        proxy_prefixes: built.proxy_prefixes.clone(),
        runner: Arc::new(tokio::sync::Mutex::new(runner)),
        client_bundle: cache.join("client-entry.js"),
        live_reload: cache.join("live-reload.js"),
        reload_tx: reload_tx.clone(),
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
    println!("  oj dev (tanstack start)");
    println!("  http://localhost:{}/", built.port);
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
    println!("  oj build (tanstack start) -> {}/dist", root.display());
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
    let mut child = tokio::process::Command::new("node")
        .arg(cache.join("runner.mjs"))
        .env("OJ_APP_ROOT", root)
        .env("NODE_ENV", "development")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn start runner (node): {e}"))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(Runner { stdin, lines: BufReader::new(stdout).lines(), _child: child })
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
    if path.rsplit('/').next().is_some_and(|seg| seg.contains('.')) {
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
            let url = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
            forward(&state, "GET".into(), url, vec![], None).await
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
