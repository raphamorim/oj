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
    client_bundle: PathBuf,
    live_reload: PathBuf,
    workspace_root: PathBuf,
    reload_tx: broadcast::Sender<()>,
    // The oj_server /__ws broadcast: the channel the Lovable editor reads boot +
    // update narration frames from (the start path's own /@oj-start/hmr socket
    // only drives the app iframe's live reload).
    ws_tx: broadcast::Sender<String>,
    css_host: Option<Arc<tokio::sync::Mutex<Runner>>>,
}

pub async fn start_dev(
    root: PathBuf,
    port: Option<u16>,
    host: Option<String>,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = root.join(".oj-cache").join("start");
    oj_server::write_start_assets(&cache)?;

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
    route_tree.await??;
    let bundle = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || bundle_client_entry(&root, &cache))
    };
    let built_fut = oj_server::DevServer {
        root: root.clone(),
        port,
        bundle: false,
        host,
        config: None,
    }
    .build_app();
    let (bundle_res, built_res) = tokio::join!(bundle, built_fut);
    bundle_res??;
    resolver.await??;
    let built = built_res?;
    let runner = spawn_start_runner(&root, &cache).await?;
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
        plugin_mw_port: built.plugin_mw_port,
        runner: Arc::new(tokio::sync::Mutex::new(runner)),
        client_bundle: cache.join("client-entry.js"),
        live_reload: cache.join("live-reload.js"),
        workspace_root: workspace_root(&root),
        reload_tx: reload_tx.clone(),
        ws_tx: built.reload_tx.clone(),
        css_host,
    });

    spawn_start_watcher(root.clone(), cache.clone(), Arc::clone(&state));

    let app = built.router.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&state),
        start_route,
    ));

    let addr = std::net::SocketAddr::from((built.host, built.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
    println!("  {} dev (tanstack start)", oj_server::oj_brand());
    let url = format!("http://localhost:{}/", built.port);
    println!("  {}", oj_server::link(&url, &oj_server::cell(&url)));
    axum::serve(listener, app).await?;
    Ok(())
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
                let _ = run_node(&root, &cache.join("gen-resolver.mjs"), "server-fn resolver");
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
                    let _ = bundle_client_entry(&r, &c);
                });
                let (_, _) = tokio::join!(client, reload_runner(&state));
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
    let cache = root.join(".oj-cache").join("start");
    oj_server::write_start_assets(&cache)?;
    generate_route_tree(&root, &cache)?;
    run_node(&root, &cache.join("gen-resolver.mjs"), "server-fn resolver")?;
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
    println!(
        "  {} build (tanstack start) -> {}/dist",
        oj_server::oj_brand(),
        root.display()
    );
    println!("  run: node dist/server.mjs");
    Ok(())
}

fn generate_route_tree(root: &Path, cache: &Path) -> anyhow::Result<()> {
    run_node(root, &cache.join("generate.mjs"), "route tree generation")
}

fn bundle_client_entry(root: &Path, cache: &Path) -> anyhow::Result<()> {
    run_node(
        root,
        &cache.join("bundle-client.mjs"),
        "client entry bundling",
    )
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
    if req.uri().path() == "/@oj-start/client-entry.js" {
        return serve_js(&state.client_bundle, "client entry").await;
    }
    if req.uri().path() == "/@oj-start/live-reload.js" {
        return serve_js(&state.live_reload, "live-reload client").await;
    }
    if let Some(abs) = req.uri().path().strip_prefix("/@oj-start/fs") {
        return serve_fs_asset(&state, abs).await;
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
        return forward(&state, method, url, headers, body).await;
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
            forward(&state, "GET".into(), document_url(&raw), vec![], None).await
        }
        Route::Pass => next.run(req).await,
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
        // A fresh directory per call: this wipes the path before creating it, so
        // two tests that pass the same label would race -- one clearing the
        // other's fixture out from under it, in whichever order they interleave.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "oj-startdev-{}-{label}-{seq}",
            std::process::id()
        ));
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
        let base = tmp("tw");
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
