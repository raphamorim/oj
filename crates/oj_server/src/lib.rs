// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Milestone-1 dev server: real React Fast Refresh.
//!
//! - Compiles TS/TSX/JSX on demand through `oj_compiler`, rewriting relative
//!   imports to canonical rooted URLs so every module has one identity.
//! - Maintains the module graph (`oj_graph`) as modules are served; file
//!   changes propagate to the nearest accepting boundary and ship as
//!   targeted `update` messages, falling back to `full-reload`.
//! - Serves Meta's react-refresh runtime (vendored via @vitejs/plugin-react's
//!   ESM build) and appends the same append-only refresh glue plugin-react
//!   uses: hoisted `$RefreshReg$`/`$RefreshSig$` locals, self-import for
//!   export-shape validation, `import.meta.hot.accept` in a microtask.
//!
//! Still deliberately unbundled (bundle-in-dev is M3); CSS updates and
//! server-side `hot.invalidate` re-propagation are M2.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use oj_cache::{CachedModule, PersistentCache};

pub mod sidecar;
use sidecar::{Sidecar, is_tailwind_css};
use oj_graph::{HmrDecision, ModuleGraph};
use oj_resolver::OjResolver;
use tokio::sync::broadcast;

const CLIENT_JS: &str = include_str!("assets/client.js");
const REFRESH_RUNTIME_JS: &str = include_str!("assets/refresh-runtime.js");
const REFRESH_PREAMBLE_JS: &str = include_str!("assets/refresh-preamble.js");
const BUNDLE_RUNTIME_JS: &str = include_str!("assets/bundle-runtime.js");
const COMPILABLE: &[&str] = &["tsx", "ts", "jsx", "js", "mjs"];

pub struct DevServer {
    pub root: PathBuf,
    /// CLI `--port`; `None` falls back to config `server.port` then 5199.
    pub port: Option<u16>,
    /// CLI `--bundle`; OR-ed with config `bundle`.
    pub bundle: bool,
}

struct ServerState {
    root: PathBuf,
    bundle: bool,
    reload_tx: broadcast::Sender<String>,
    graph: Mutex<ModuleGraph>,
    resolver: Arc<OjResolver>,
    cache: PersistentCache,
    /// url -> (content key, output). Content key re-checked per request, so
    /// this needs no watcher-driven invalidation to stay correct.
    memory: Mutex<HashMap<String, (String, Arc<CachedModule>)>>,
    /// Per-url compile locks: concurrent requests (or crawl vs request) for
    /// the same module coalesce into one compile.
    compile_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Flips true when the eager startup crawl has the full graph.
    crawl_done: tokio::sync::watch::Receiver<bool>,
    /// Out-of-root files our resolver legitimately resolved (workspace
    /// packages etc). /@fs/ requests are served ONLY from this set, so the
    /// scheme cannot be used for directory traversal.
    fs_allow: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    /// Monotonic patch counter; the client detects a gap in the sequence
    /// (a dropped WS frame after a backgrounded tab / reconnect) and reloads
    /// rather than applying patches onto a diverged module graph.
    patch_seq: std::sync::atomic::AtomicU64,
    /// Assembled bundle-mode chunk: (etag, bytes), memoized until the
    /// watcher sees a change (FBM's staleness model: fresh reloads do zero
    /// assembly work and mostly answer 304).
    chunk_cache: Mutex<Option<(String, Arc<String>)>>,
    /// Dedicated disk-cache writer (single thread, bounded queue).
    cache_writes: tokio::sync::mpsc::Sender<(String, Arc<CachedModule>)>,
    /// Lazily-spawned Tailwind v4 Node sidecar + the css urls it owns
    /// (regenerated whenever any source file changes).
    tailwind: tokio::sync::OnceCell<std::sync::Arc<Sidecar>>,
    tailwind_urls: Mutex<std::collections::HashSet<String>>,
    /// Module list persisted by the previous session's crawl: lets a warm
    /// start emit the full modulepreload list immediately, without HTML
    /// ever waiting on the live crawl.
    preload_snapshot: Vec<String>,
    /// `server.proxy` rules: (path prefix, entry). Empty = no proxying.
    proxy: Vec<(String, oj_config::ProxyEntry)>,
    /// Client for forwarding proxied requests.
    http: reqwest::Client,
}

impl DevServer {
    pub async fn run(self) -> anyhow::Result<()> {
        let root = self
            .root
            .canonicalize()
            .with_context(|| format!("app root not found: {}", self.root.display()))?;

        // Load oj.config.* — the source for proxy/port/host/bundle/envPrefix.
        let config = oj_config::load(&root).map_err(|e| anyhow::anyhow!("{e}"))?;

        // Load .env files (dev mode) and install the import.meta.env defines
        // before any module compiles. envPrefix from config overrides VITE_.
        let env_prefix = config.env_prefix.as_deref().unwrap_or("VITE_");
        let env_dir = config.env_dir.as_deref().map(|d| root.join(d)).unwrap_or_else(|| root.clone());
        let env = oj_env::load(&env_dir, "development");
        oj_compiler::set_import_meta_env(oj_env::import_meta_env_defines(
            &env,
            "development",
            true,
            config.base.as_deref().unwrap_or("/"),
            env_prefix,
        ));

        // Precedence: CLI flag > config > built-in default.
        let server_cfg = config.server.clone().unwrap_or_default();
        let port = self.port.or(server_cfg.port).unwrap_or(5199);
        let bundle = self.bundle || config.bundle.unwrap_or(false);
        let host: std::net::IpAddr = match server_cfg.host.as_deref() {
            Some("0.0.0.0") | Some("true") => [0, 0, 0, 0].into(),
            Some(h) => h.parse().unwrap_or([127, 0, 0, 1].into()),
            None => [127, 0, 0, 1].into(),
        };
        let proxy: Vec<(String, oj_config::ProxyEntry)> =
            server_cfg.proxy.clone().unwrap_or_default().into_iter().collect();

        let started = Instant::now();
        let (reload_tx, _) = broadcast::channel::<String>(64);
        let (crawl_tx, crawl_rx) = tokio::sync::watch::channel(false);
        let (write_tx, mut write_rx) =
            tokio::sync::mpsc::channel::<(String, Arc<CachedModule>)>(65536);
        let state = Arc::new(ServerState {
            root: root.clone(),
            bundle,
            reload_tx,
            graph: Mutex::new(ModuleGraph::new()),
            resolver: Arc::new(OjResolver::new(&root)),
            cache: PersistentCache::new(
                root.join(".oj-cache"),
                env!("CARGO_PKG_VERSION"),
            ),
            memory: Mutex::new(HashMap::new()),
            compile_locks: Mutex::new(HashMap::new()),
            crawl_done: crawl_rx,
            tailwind: tokio::sync::OnceCell::new(),
            tailwind_urls: Mutex::new(std::collections::HashSet::new()),
            fs_allow: Arc::new(Mutex::new(std::collections::HashSet::new())),
            patch_seq: std::sync::atomic::AtomicU64::new(0),
            chunk_cache: Mutex::new(None),
            cache_writes: write_tx,
            preload_snapshot: load_graph_snapshot(&root),
            proxy,
            http: reqwest::Client::new(),
        });
        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                while let Some((key, module)) = write_rx.blocking_recv() {
                    state.cache.put(&key, &module);
                }
            });
        }
        spawn_watcher(Arc::clone(&state));
        spawn_crawl(Arc::clone(&state), crawl_tx);

        let mut app = Router::new()
            .route("/@oj/client.js", get(|| async { js(CLIENT_JS) }))
            .route("/@oj/refresh-runtime.js", get(|| async { js(REFRESH_RUNTIME_JS) }))
            .route("/@oj/refresh-preamble.js", get(|| async { js(REFRESH_PREAMBLE_JS) }))
            .route("/@oj/bundle-runtime.js", get(|| async { js(BUNDLE_RUNTIME_JS) }))
            .route("/@oj/chunk.js", get(serve_chunk))
            .route("/@oj/patch.js", get(serve_patch))
            .route("/__ws", get(ws_upgrade))
            .fallback(get(serve_path));
        // Proxy runs ahead of routing so configured prefixes (/api, ...) are
        // forwarded before hitting the file fallback.
        if !state.proxy.is_empty() {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                proxy_middleware,
            ));
        }
        let proxy_prefixes: Vec<String> =
            state.proxy.iter().map(|(p, _)| p.clone()).collect();
        let app = app.with_state(state);

        let addr = SocketAddr::from((host, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("cannot bind {addr}"))?;
        println!("  oj dev server");
        println!("  root: {}", root.display());
        println!("  http://localhost:{}/", port);
        if !proxy_prefixes.is_empty() {
            println!("  proxy: {}", proxy_prefixes.join(", "));
        }
        println!("  ready in {:?}", started.elapsed());
        axum::serve(listener, app).await?;
        Ok(())
    }
}

fn js(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], body).into_response()
}

/// Static server for a production build (`oj preview`). Serves `dir`,
/// strips `base`, refuses traversal, and falls back to `index.html` for
/// extensionless routes (SPA client routing).
pub async fn preview(dir: PathBuf, port: u16, base: String) -> anyhow::Result<()> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("build dir not found: {} (run `oj build` first)", dir.display()))?;
    let state = Arc::new((dir.clone(), base));
    let app = Router::new().fallback(get(preview_serve)).with_state(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))?;
    println!("  oj preview");
    println!("  serving: {}", dir.display());
    println!("  http://localhost:{port}/");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Map a request path (with the base stripped) to a file under the build
/// dir, or `None` for a traversal attempt. Empty -> `index.html`.
fn preview_rel<'a>(path: &'a str, base: &str) -> Option<String> {
    let trimmed = path.strip_prefix(base.trim_end_matches('/')).unwrap_or(path);
    let rel = trimmed.trim_start_matches('/');
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(if rel.is_empty() { "index.html".to_string() } else { rel.to_string() })
}

async fn preview_serve(
    State(state): State<Arc<(PathBuf, String)>>,
    uri: Uri,
) -> Response {
    let (dir, base) = &*state;
    let Some(rel) = preview_rel(uri.path(), base) else {
        return (StatusCode::FORBIDDEN, "oj: path traversal denied").into_response();
    };
    let file = dir.join(&rel);
    let ext = Path::new(&rel).extension().and_then(|e| e.to_str()).unwrap_or("");

    // Existing file -> serve it. Otherwise, extensionless routes fall back to
    // index.html so client-side routing works on deep links.
    let (target, ctype) = if file.is_file() {
        (file, content_type(ext))
    } else if ext.is_empty() {
        (dir.join("index.html"), "text/html; charset=utf-8")
    } else {
        return (StatusCode::NOT_FOUND, format!("oj: not found: {rel}")).into_response();
    };

    match tokio::fs::read(&target).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "oj: not found").into_response(),
    }
}

async fn ws_upgrade(
    State(state): State<Arc<ServerState>>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        let mut rx = state.reload_tx.subscribe();
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

/// Forward requests whose path matches a `server.proxy` prefix to the
/// configured target (longest prefix wins), otherwise pass through. Supports
/// `changeOrigin`, `ws` (marker only for now), and `^from -> to` rewrite.
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

    // Compose the target URL: target + (rewritten) path + query.
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
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let target = format!("{}{}{}", entry.target().trim_end_matches('/'), fwd_path, query);

    let method = req.method().clone();
    let req_headers = req.headers().clone();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 100 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("oj proxy: body read: {e}")).into_response()
        }
    };

    let mut out = state.http.request(method, &target).body(body_bytes.to_vec());
    for (name, value) in req_headers.iter() {
        // Host is set by reqwest per target when changeOrigin; else forward.
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
                // reqwest already decompressed; drop framing headers that
                // would now be wrong.
                if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
                    continue;
                }
                response.headers_mut().insert(name, value.clone());
            }
            response
        }
        Err(e) => {
            let via = if entry.ws() { " (ws proxying not yet supported)" } else { "" };
            (StatusCode::BAD_GATEWAY, format!("oj proxy to {}{} failed: {e}", prefix, via))
                .into_response()
        }
    }
}

async fn serve_path(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    let file = if let Some(abs) = uri.path().strip_prefix("/@fs") {
        let candidate = PathBuf::from(abs);
        let allowed = {
            let allow = state.fs_allow.lock().unwrap();
            allow.iter().any(|root| candidate.starts_with(root))
        };
        if !allowed {
            return (StatusCode::FORBIDDEN, "oj: /@fs path not allow-listed").into_response();
        }
        candidate
    } else {
        match locate(&state.root, rel) {
            Some(file) => file,
            None => {
                return (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}"))
                    .into_response();
            }
        }
    };

    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if let Some(kind) = query_asset_kind(uri.query()) {
        let url = url_of(&state.root, &file);
        return match asset_module(&file, &url, kind).await {
            Ok(js) => (
                [(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }
    if matches!(ext, "css" | "scss" | "sass")
        && uri.query().is_some_and(|q| q.contains("import"))
    {
        let url = url_of(&state.root, &file);
        return serve_css_wrapper(&state, &file, &url).await;
    }
    if COMPILABLE.contains(&ext) {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    // JSON imported from JS becomes a module; JSON under publicDir stays raw.
    if ext == "json" && !file.starts_with(state.root.join("public")) {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }

    match tokio::fs::read(&file).await {
        Ok(bytes) if ext == "html" => {
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            let html = if state.bundle {
                inject_bundle_scripts(raw)
            } else {
                inject_module_preloads(inject_dev_scripts(raw), &state)
            };
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
        }
        Ok(bytes) if ext == "css" => {
            let source = String::from_utf8_lossy(&bytes).into_owned();
            if is_tailwind_css(&source) {
                let url = url_of(&state.root, &file);
                return match compile_tailwind(&state, &url, &source).await {
                    Ok(css) => {
                        ([(header::CONTENT_TYPE, "text/css"), (header::CACHE_CONTROL, "no-cache")], css)
                            .into_response()
                    }
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
        Err(err) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: read error: {err}"))
                .into_response()
        }
    }
}

/// The preamble must be the first module script in the document: module
/// scripts execute in document order, and injectIntoGlobalHook has to run
/// before any module pulls in React.
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
            // Push to the overlay too: during a hot update the failed module
            // is fetched via dynamic import and the 500 body is never shown.
            let _ = state.reload_tx.send(
                serde_json::json!({ "type": "error", "message": err.clone() }).to_string(),
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };

    // The content key IS the etag: reloads revalidate 5k modules as cheap
    // 304s instead of re-downloading bodies. ?t= requests are one-shot HMR
    // fetches — no etag there.
    let etag = format!("\"{key}\"");
    if query.is_none() {
        if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
            if inm == etag {
                return (
                    StatusCode::NOT_MODIFIED,
                    [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache".to_string())],
                )
                    .into_response();
            }
        }
    }

    let mut body = module.code.clone();
    if !state.bundle {
        body.push_str(&hot_glue(url, query, module.is_boundary));
    }
    if let Some(map_url) = &module.map_data_url {
        // Glue is append-only, so original line mappings stay exact.
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

/// Compile-or-cache one module: memory -> disk -> compile, coalescing
/// concurrent callers (HTTP requests and the startup crawl) per url.
/// Also re-applies the module's graph edges — the graph is in-memory only
/// and empty after a restart, cache hits included.
async fn ensure_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<(String, Arc<CachedModule>), String> {
    let source = tokio::fs::read_to_string(file)
        .await
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
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let mode = if state.bundle { "bundle" } else { "dev" };
    let key = state.cache.key(source.as_bytes(), url, mode);

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry(url.to_string()).or_default())
    };
    let _guard = lock.lock().await;

    // A coalesced waiter arrives here after the winner finished: re-check.
    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }
    if let Some(module) = state.cache.get(&key) {
        let module = Arc::new(module);
        memory_put(state, url, &key, &module);
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let file_owned = file.to_path_buf();
    let url_owned = url.to_string();
    let is_dep = url.contains("/node_modules/") || url.starts_with("/@fs/");
    let bundle = state.bundle;
    let ext = file.extension().and_then(|e| e.to_str());
    let is_css = matches!(ext, Some("css") | Some("scss") | Some("sass"));
    let is_json = ext == Some("json");
    let compiled = tokio::task::spawn_blocking(move || -> Result<CachedModule, String> {
        if is_json {
            // JSON as a module: a JS body exporting default + named keys.
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
            });
        }
        if is_css {
            // Sass/SCSS -> CSS first (sibling @use/@import resolve from dir).
            let css_src = if oj_css::is_sass(&url_owned) {
                oj_css::compile_sass(&source, Some(&dir))?
            } else {
                source.clone()
            };
            let output = oj_css::compile_css(&url_owned, &css_src, false)?;
            return Ok(CachedModule {
                is_boundary: true, // JS-imported css self-accepts its updates
                kind: "css".into(),
                code: output.css,
                map_data_url: None,
                imports: Vec::new(),
                require_map: Vec::new(),
                css_exports: output.exports.unwrap_or_default(),
                fs_allow: Vec::new(),
            });
        }
        let mut rewrite =
            |spec: &str| rewrite_specifier(&root, &dir, &resolver, &fs_allow, spec, !bundle);
        if bundle {
            let factory =
                oj_compiler::bundle::compile_factory(&file_owned, &url_owned, &source, &mut rewrite)
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
                imports: factory.imports,
                require_map: factory.require_map,
                css_exports: Vec::new(),
            })
        } else {
            let output = if is_dep {
                oj_compiler::cjs::compile_dep(&file_owned, &url_owned, &source, &mut rewrite)
            } else {
                oj_compiler::compile_module(
                    &file_owned,
                    &source,
                    &oj_compiler::CompileOptions::dev(),
                    Some(&mut rewrite),
                )
            }
            .map_err(|err| format!("compile error:\n{err}"))?;
            Ok(CachedModule {
                is_boundary: !is_dep && output.has_refresh_registrations(),
                code: output.code,
                map_data_url: output.map_data_url,
                fs_allow: fs_allow_from(&output.imports),
                imports: output.imports,
                kind: String::new(),
                require_map: Vec::new(),
                css_exports: Vec::new(),
            })
        }
    })
    .await;

    let module = match compiled {
        Ok(Ok(module)) => Arc::new(module),
        Ok(Err(err)) => return Err(err),
        Err(join_err) => return Err(format!("compiler task failed: {join_err}")),
    };
    // Disk-cache writes go through one dedicated writer thread: keeps
    // serialize+write+rename off the compile tasks WITHOUT flooding the
    // blocking pool (fire-and-forget spawn_blocking per write measurably
    // regressed cold start). Best-effort: a full queue just drops the write.
    let _ = state.cache_writes.try_send((key.clone(), Arc::clone(&module)));
    memory_put(state, url, &key, &module);
    register_in_graph(state, url, &module);
    Ok((key, module))
}

fn memory_get(state: &ServerState, url: &str, key: &str) -> Option<Arc<CachedModule>> {
    let memory = state.memory.lock().unwrap();
    memory.get(url).filter(|(k, _)| k == key).map(|(_, m)| Arc::clone(m))
}

fn memory_put(state: &ServerState, url: &str, key: &str, module: &Arc<CachedModule>) {
    state
        .memory
        .lock()
        .unwrap()
        .insert(url.to_string(), (key.to_string(), Arc::clone(module)));
}

/// The package a file belongs to: nearest ancestor with a package.json
/// (fallback: the file's own directory). This is the /@fs trust boundary —
/// pulling in one file from a resolved dependency trusts that package's
/// shipped assets (e.g. a wasm loaded at runtime via new URL(import.meta.url)).
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

/// Package-root prefixes for a module's /@fs/ imports (query stripped).
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
    let mut graph = state.graph.lock().unwrap();
    let local_imports: Vec<PathBuf> = module
        .imports
        .iter()
        .filter(|s| s.starts_with('/') && !s.starts_with("/@oj/"))
        .map(|s| PathBuf::from(s.split('?').next().unwrap_or(s)))
        .collect();
    graph.set_imports(Path::new(url), &local_imports);
    graph.set_self_accepting(Path::new(url), module.is_boundary);
}

/// JS-imported css: a style-injecting ES module with the scoped class map
/// as its default export. Self-accepting, so edits hot-swap through the
/// normal module-update flow with React state intact.
async fn serve_css_wrapper(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    let (_, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            let _ = state.reload_tx.send(
                serde_json::json!({ "type": "error", "message": err.clone() }).to_string(),
            );
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
        [(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")],
        body,
    )
        .into_response()
}

/// Compile tailwind-flavored css through the Node sidecar. Never cached:
/// the output depends on class candidates across the whole app, not on the
/// css file's own content.
async fn compile_tailwind(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let sidecar = state
        .tailwind
        .get_or_try_init(|| Sidecar::spawn(&state.root))
        .await
        .map_err(|e| e.to_string())?;
    let css = sidecar.compile(source, url).await?;
    state.tailwind_urls.lock().unwrap().insert(url.to_string());
    Ok(css)
}

/// Handle messages from the dev client (currently only `invalidate`).
fn handle_client_message(state: &Arc<ServerState>, text: &str) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else { return };
    if msg["type"] == "invalidate" {
        let Some(path) = msg["path"].as_str() else { return };
        let decision =
            state.graph.lock().unwrap().propagate_update_from_importers(Path::new(path));
        let reply = match decision {
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
        };
        let _ = state.reload_tx.send(reply.to_string());
    } else if msg["type"] == "custom" {
        // A client `import.meta.hot.send(event, data)`. With no plugin system
        // yet, broadcast it to all clients so hot.on(event) listeners (this
        // tab and others) receive it — enough for round-trip messaging.
        if msg["event"].is_string() {
            let _ = state.reload_tx.send(
                serde_json::json!({
                    "type": "custom",
                    "event": msg["event"],
                    "data": msg["data"],
                })
                .to_string(),
            );
        }
    }
}

/// The per-module HMR/Fast Refresh glue, following @vitejs/plugin-react's
/// current append-only wrapper: `$RefreshReg$`/`$RefreshSig$` are hoisted
/// function declarations, so the transform's module-body calls resolve to
/// these locals instead of the window stubs; the module imports itself
/// (with the same ?t query when hot-updated) to hand its export namespace
/// to the boundary validator.
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
    throw new Error("oj: Fast Refresh preamble missing — was index.html served by oj?");
  }}
  const currentExports = __oj_currentExports;
  // Register synchronously during module evaluation (NOT in a microtask):
  // a fast second edit must find the accept callback the instant the first
  // edit's dynamic import resolves, or it snapshots an empty list and is
  // silently dropped. This mirrors Vite's shared/hmr.ts.
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

/// `./App` from `<root>/src` -> `/src/App.tsx`; `react` -> the resolved
/// node_modules entry as a rooted URL. Rooted/virtual/absolute-URL
/// specifiers pass through untouched.
fn rewrite_specifier(
    root: &Path,
    dir: &Path,
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    spec: &str,
    css_import_marker: bool,
) -> Option<String> {
    if spec.starts_with('/') || spec.contains("://") {
        return None;
    }

    // Query-suffixed imports (`./x.wasm?url`, `./x.txt?raw`, `./x.png?inline`,
    // `./w.ts?worker`): resolve the base file, keep the marker; the server
    // answers with a JS module (url string / contents / data URI / Worker
    // factory).
    if let Some((base, query)) = spec.split_once('?') {
        if matches!(query, "url" | "raw" | "inline" | "worker" | "sharedworker") {
            let resolved = rewrite_specifier(root, dir, resolver, fs_allow, base, false)
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
        // TS convention: `import "./x.js"` resolves the sibling `./x.ts`
        // (also .tsx/.jsx). If the literal .js/.jsx target is absent, retarget.
        if !joined.is_file() {
            if let Some(ext) = joined.extension().and_then(|e| e.to_str()) {
                if ext == "js" || ext == "jsx" {
                    for cand in ["ts", "tsx"] {
                        let alt = joined.with_extension(cand);
                        if alt.is_file() {
                            joined = alt;
                            break;
                        }
                    }
                }
            }
        }
        let quick = if joined.is_file() {
            Some(joined)
        } else if joined.extension().is_none() {
            COMPILABLE.iter().map(|ext| joined.with_extension(ext)).find(|c| c.is_file())
        } else {
            None
        };
        if let Some(p) = quick {
            let url = url_of(root, &p);
            // JS-imported css is served as a style-injecting JS module; the
            // ?import marker distinguishes it from <link> requests.
            if css_import_marker
                && (url.ends_with(".css") || url.ends_with(".scss") || url.ends_with(".sass"))
            {
                return Some(format!("{url}?import"));
            }
            return Some(url);
        }
        // Directories, `./x.js` -> `x.ts`, etc.: let the real resolver try.
    }

    match resolver.resolve(dir, spec) {
        Ok(resolved) if resolved.starts_with(root) => Some(url_of(root, &resolved)),
        Ok(resolved) => {
            // Outside the served root (workspace packages, hoisted installs):
            // trust the whole package and serve it under /@fs/.
            fs_allow.lock().unwrap().insert(package_root(&resolved));
            Some(url_of(root, &resolved))
        }
        Err(err) => {
            if !(spec.starts_with("./") || spec.starts_with("../")) {
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

/// Map a URL path to a file under root, refusing traversal and probing
/// TS-first extensions for extensionless imports (`/src/App` -> `App.tsx`).
fn locate(root: &Path, rel: &str) -> Option<PathBuf> {
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
    // Vite-style publicDir: assets under <root>/public are served at root.
    let public = root.join("public").join(rel);
    if public.is_file() {
        return Some(public);
    }
    None
}

/// Which query-suffix asset module a request wants, if any.
fn query_asset_kind(query: Option<&str>) -> Option<&'static str> {
    let q = query?;
    for kind in ["url", "raw", "inline", "worker", "sharedworker"] {
        if q.split('&').any(|kv| kv == kind) {
            return Some(kind);
        }
    }
    None
}

/// Build the JS module for a `?url` / `?raw` / `?inline` asset import.
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
            let bytes = tokio::fs::read(file).await.map_err(|e| format!("read: {e}"))?;
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            let mime = content_type(ext).split(';').next().unwrap_or("application/octet-stream");
            let data_uri = format!("data:{mime};base64,{}", base64_encode(&bytes));
            Ok(format!("export default {data_uri:?};\n"))
        }
        // `?worker` / `?sharedworker`: a factory that constructs a module
        // Worker over the (separately compiled+served) worker script.
        "worker" | "sharedworker" => {
            let ctor = if kind == "sharedworker" { "SharedWorker" } else { "Worker" };
            Ok(format!(
                "export default function () {{ return new {ctor}({clean_url:?}, {{ type: \"module\" }}); }}\n"
            ))
        }
        _ => Err(format!("unknown asset query: {kind}")),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
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
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// `<link rel="modulepreload">` for every known module, so the browser
/// fetches the whole graph in parallel instead of discovering it one
/// import-depth round trip at a time. This is what replaces bundling while
/// dev stays native-ESM.
fn inject_module_preloads(html: String, state: &ServerState) -> String {
    // Live graph once the crawl finished; last session's snapshot before
    // that. Never block HTML on compilation — a stale snapshot just means a
    // few extra or missing preloads, and imports still resolve normally.
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
            // Stylesheets live in the graph under their clean url but are
            // served as JS only with the ?import marker.
            if p.ends_with(".css") || p.ends_with(".scss") || p.ends_with(".sass") {
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

/// Bundle mode: runtime + chunk replace the app's own module script tags.
fn inject_bundle_scripts(html: String) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    // Drop every `<script type="module" src="/...">...</script>` tag; the
    // chunk executes those entries via __oj_start.
    while let Some(start) = rest.find("<script") {
        let Some(tag_close) = rest[start..].find('>') else { break };
        let tag = &rest[start..start + tag_close];
        if tag.contains("type=\"module\"") && tag.contains("src=\"/") {
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
    // Fresh case: cached bytes + etag, so an unchanged reload is a 304 and
    // never re-assembles (the FBM memoryFiles model).
    if let Some((etag, body)) = state.chunk_cache.lock().unwrap().clone() {
        return chunk_response(&headers, etag, body);
    }

    // The chunk needs the full graph: wait for the eager crawl.
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

    // Single-flight assembly: coalesce concurrent cold reloads.
    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry("/@oj/chunk.js".into()).or_default())
    };
    let _guard = lock.lock().await;
    if let Some((etag, body)) = state.chunk_cache.lock().unwrap().clone() {
        return chunk_response(&headers, etag, body);
    }

    let mut chunk = String::new();
    for url in &urls {
        match registration_for(&state, url).await {
            Ok(registration) => chunk.push_str(&registration),
            Err(err) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: chunk: {err}"))
                    .into_response();
            }
        }
    }
    for entry in html_entries(&state.root) {
        chunk.push_str(&format!("__oj_start({entry:?});\n"));
    }
    let etag = format!("\"{}\"", state.cache.key(chunk.as_bytes(), "/@oj/chunk.js", "chunk"));
    let body = Arc::new(chunk);
    *state.chunk_cache.lock().unwrap() = Some((etag.clone(), Arc::clone(&body)));
    chunk_response(&headers, etag, body)
}

fn chunk_response(headers: &HeaderMap, etag: String, body: Arc<String>) -> Response {
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache".to_string())],
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
        .into_response()}

/// `?m=url1,url2&t=...` -> re-registrations for the changed modules.
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
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: patch: {err}"))
                    .into_response();
            }
        }
    }
    (
        [(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")],
        patch,
    )
        .into_response()
}

/// One `__oj_register(...)` statement for a module, compiling if needed.
async fn registration_for(state: &Arc<ServerState>, url: &str) -> Result<String, String> {
    let file = if let Some(abs) = url.strip_prefix("/@fs") {
        PathBuf::from(abs)
    } else {
        let rel = url.trim_start_matches('/');
        locate(&state.root, rel).ok_or_else(|| format!("no such module: {url}"))?
    };
    let (_, module) = ensure_module(state, &file, url).await?;
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
        return Ok(format!(
            "__oj_register({url:?}, \"esm\", {{}}, function(module, __oj_exports, __oj_require) {{\n             __oj_esm(__oj_exports, {{ \"default\": () => __oj_css_default }});\n             var __oj_css_default = {exports};\n             __oj_inject_css({url:?}, {css});\n             }});\n",
            css = serde_json::Value::String(module.code.clone()),
        ));
    }

    // Parameter names are kind-specific: CJS bodies reference
    // `exports`/`require` directly, ESM factories the __oj_* forms.
    let params = if module.kind == "cjs" {
        "module, exports, require"
    } else {
        "module, __oj_exports, __oj_require"
    };
    Ok(format!(
        "__oj_register({url:?}, {kind:?}, {deps}, function({params}) {{\n{body}\n}});\n",
        kind = module.kind,
        deps = serde_json::Value::Object(deps),
        body = module.code,
    ))
}

fn urldecode(input: &str) -> String {
    // Only %2F and %2C realistically appear in our module lists.
    input.replace("%2F", "/").replace("%2f", "/").replace("%2C", ",").replace("%2c", ",")
}

/// Module-script entry URLs from the app's index.html.
fn html_entries(root: &Path) -> Vec<String> {
    let Ok(html) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for tag_start in html.match_indices("<script").map(|(i, _)| i) {
        let Some(tag_end) = html[tag_start..].find('>') else { continue };
        let tag = &html[tag_start..tag_start + tag_end];
        if !tag.contains("type=\"module\"") {
            continue;
        }
        if let Some(src_at) = tag.find("src=\"") {
            let rest = &tag[src_at + 5..];
            if let Some(end) = rest.find('"') {
                let src = &rest[..end];
                if src.starts_with('/') {
                    entries.push(src.to_string());
                }
            }
        }
    }
    entries
}

/// Startup crawl: compile the entire entry-reachable graph in parallel so
/// first paint never waits on request-driven, depth-serialized discovery.
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
                    let ok = { let a = state.fs_allow.lock().unwrap();
                        a.iter().any(|r| f.starts_with(r)) };
                    if !ok { continue; }
                    f
                } else {
                    let rel = url.trim_start_matches('/').to_string();
                    match locate(&state.root, &rel) { Some(f) => f, None => continue }
                };
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !COMPILABLE.contains(&ext) && !matches!(ext, "css" | "scss" | "sass" | "json") {
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
                        let import =
                            import.split('?').next().unwrap_or(&import).to_string();
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
        println!("oj: eager graph ready — {} modules in {:?}", paths.len(), started.elapsed());
        save_graph_snapshot(&state.root, &paths);
        let _ = done_tx.send(true);
    });
}

fn snapshot_path(root: &Path) -> PathBuf {
    root.join(".oj-cache").join("graph-snapshot.json")
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
        if let Err(err) = watcher.watch(&state.root, RecursiveMode::Recursive) {
            eprintln!("oj: cannot watch {}: {err}", state.root.display());
            return;
        }

        // Trailing/coalescing debounce. Editor and OS writes fire several
        // events per save; we collect a burst and process it once after a
        // short quiet gap. Crucially this NEVER drops an event: a leading
        // "skip if within Nms of the last send" debounce silently loses a
        // second edit whose event lands in the shadow of a trailing event
        // from the first — the bug that made consecutive edits fail.
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            // Block for the first event of a burst.
            let first = match rx.recv() {
                Ok(Ok(ev)) => ev,
                Ok(Err(_)) => continue,
                Err(_) => break, // channel closed
            };
            let mut paths: std::collections::HashSet<PathBuf> =
                first.paths.into_iter().collect();
            // Drain the rest of the burst until things go quiet.
            loop {
                match rx.recv_timeout(Duration::from_millis(30)) {
                    Ok(Ok(ev)) => paths.extend(ev.paths),
                    Ok(Err(_)) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let paths: Vec<PathBuf> = paths.into_iter().collect();
            let messages = decide(&state, &paths);
            if messages.is_empty() {
                continue;
            }
            // Anything changed: the assembled chunk is stale.
            *state.chunk_cache.lock().unwrap() = None;
            for message in messages {
                let _ = state.reload_tx.send(message);
            }
        }
    });
}

/// Turn a batch of changed paths into HMR messages (empty if irrelevant).
fn decide(state: &ServerState, paths: &[PathBuf]) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    let mut updates: Vec<serde_json::Value> = Vec::new();

    // Any source change can mint new utility classes: refresh tailwind css.
    let source_changed = paths.iter().any(|p| {
        !p.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist"
        })
            && p.extension()
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
        // Deps change via installs, not saves; watching them is pure noise.
        // .oj-cache and dist are our own outputs.
        if path.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist"
        }) {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "css" {
            let url = url_of(&state.root, path);
            if !state.graph.lock().unwrap().contains(Path::new(&url)) {
                // Only referenced via <link>: hot-swap the stylesheet.
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
            // JS-imported css: falls through to module propagation below.
        }
        if ext == "html" {
            println!("oj: change {} -> full-reload", path.display());
            messages.push(
                serde_json::json!({ "type": "full-reload", "reason": path.display().to_string() })
                    .to_string(),
            );
            return messages;
        }
        if !COMPILABLE.contains(&ext) && !matches!(ext, "css" | "scss" | "sass" | "json") {
            continue;
        }

        let url = url_of(&state.root, path);
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
        let decision = state.graph.lock().unwrap().propagate_update(Path::new(&url));
        match decision {
            HmrDecision::Update { boundaries } => {
                println!("oj: change {url} -> update {boundaries:?}");
                let timestamp = now_millis() as u64;
                updates.extend(boundaries.iter().map(|b| {
                    let mut path = format!("{}", b.display());
                    if path.ends_with(".css") {
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
    fn normalize_resolves_parent_components() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
    }

    #[test]
    fn preview_rel_maps_base_and_guards_traversal() {
        assert_eq!(preview_rel("/", "/").as_deref(), Some("index.html"));
        assert_eq!(preview_rel("/assets/x.js", "/").as_deref(), Some("assets/x.js"));
        // base stripping
        assert_eq!(preview_rel("/app/assets/x.js", "/app/").as_deref(), Some("assets/x.js"));
        assert_eq!(preview_rel("/app/", "/app/").as_deref(), Some("index.html"));
        // traversal denied
        assert_eq!(preview_rel("/../etc/passwd", "/"), None);
    }
}
