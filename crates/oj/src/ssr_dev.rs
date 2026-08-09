// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev-server SSR (`oj dev --ssr <entry>`).
//!
//! Build-and-run SSR with client hydration: on each request the server bundle
//! is (re)built if a source file changed, then executed in Node to produce
//! HTML. When a sibling `*-client.*` entry exists, a browser bundle is built
//! alongside and injected as a `<script type="module">`, so React hydrates the
//! server-rendered markup and the page becomes interactive. A WebSocket
//! triggers a full page reload on edit.
//!
//! This is the pre-module-runner style of dev SSR — honest and useful, but
//! coarse: it rebuilds whole bundles per change, spawns Node per render, and
//! full-reloads (no fine-grained SSR HMR). The Environment API module runner
//! is the larger remaining work.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context;
use axum::{
    Router,
    extract::{Path as AxumPath, State, WebSocketUpgrade, ws::Message},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tokio::sync::broadcast;

const CLIENT: &str = r#"<script>
(function c(){var w=new WebSocket("ws://"+location.host+"/__ojssr");
w.onmessage=function(){location.reload()};w.onclose=function(){setTimeout(c,1000)}})();
</script>"#;

struct SsrState {
    root: PathBuf,
    entry: String,
    server_dir: PathBuf,
    server_stem: String,
    /// A sibling `*-client.*` hydration entry, if one exists.
    client: Option<Client>,
    dirty: AtomicBool,
    version: AtomicU64,
    reload_tx: broadcast::Sender<()>,
}

struct Client {
    entry: String,
    stem: String,
    dir: PathBuf,
}

pub async fn ssr_dev(root: PathBuf, entry: String, port: u16) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;
    let server_stem = stem_of(&entry, "server");
    let bundle_dir = root.join(".oj-cache").join("ssr");

    // Hydration is opt-in by convention: a `*-client.*` sibling of the server
    // entry. Without one, we serve inert SSR HTML (still useful).
    let client = derive_client_entry(&root, &entry).map(|entry| Client {
        stem: stem_of(&entry, "client"),
        dir: bundle_dir.join("client"),
        entry,
    });

    let (reload_tx, _) = broadcast::channel(16);
    let state = Arc::new(SsrState {
        server_dir: bundle_dir.join("server"),
        server_stem,
        client,
        root,
        entry,
        dirty: AtomicBool::new(true),
        version: AtomicU64::new(0),
        reload_tx,
    });

    spawn_watcher(Arc::clone(&state));

    let app = Router::new()
        .route("/__ojssr", get(ws))
        .route("/@oj/{*path}", get(serve_client_asset))
        .fallback(get(render))
        .with_state(Arc::clone(&state));
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("  oj dev (ssr)");
    println!("  entry:  {}", state.entry);
    match &state.client {
        Some(c) => println!("  client: {} (hydration on)", c.entry),
        None => println!("  client: none (SSR only; add a *-client entry to hydrate)"),
    }
    println!("  http://localhost:{port}/");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Rebuild the server (and client) bundles if a source file changed.
async fn rebuild(state: &SsrState) -> anyhow::Result<()> {
    crate::build::build_ssr(&state.root, &state.server_dir, &state.entry, false).await?;
    if let Some(c) = &state.client {
        crate::build::build_client(&state.root, &c.dir, &c.entry).await?;
    }
    Ok(())
}

async fn render(State(state): State<Arc<SsrState>>) -> Response {
    if state.dirty.swap(false, Ordering::SeqCst) {
        if let Err(e) = rebuild(&state).await {
            // Leave dirty so the next request retries a fresh build.
            state.dirty.store(true, Ordering::SeqCst);
            return error_page(&format!("SSR build failed:\n{e}"));
        }
    }
    let bundle = state.server_dir.join(format!("{}.mjs", state.server_stem));
    let v = state.version.fetch_add(1, Ordering::Relaxed);
    // Cache-bust the import so a rebuilt bundle is re-evaluated.
    let script = format!(
        "import('file://{}?v={v}').then(m => process.stdout.write(String(m.render())))",
        bundle.display()
    );
    let out = tokio::process::Command::new("node")
        .args(["--input-type=module", "-e", &script])
        .current_dir(&state.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            Html(page(&state, &String::from_utf8_lossy(&o.stdout))).into_response()
        }
        Ok(o) => error_page(&format!("SSR render failed:\n{}", String::from_utf8_lossy(&o.stderr))),
        Err(e) => error_page(&format!("could not spawn node: {e}")),
    }
}

/// Wrap the server-rendered `body` in an HTML document, adding the client
/// hydration script (and its stylesheet, if any) plus the reload client.
fn page(state: &SsrState, body: &str) -> String {
    let (head, script) = match &state.client {
        Some(c) => {
            let css = c.dir.join(format!("{}.css", c.stem)).is_file();
            let link = if css {
                format!("<link rel=\"stylesheet\" href=\"/@oj/{}.css\">", c.stem)
            } else {
                String::new()
            };
            (link, format!("<script type=\"module\" src=\"/@oj/{}.js\"></script>", c.stem))
        }
        None => (String::new(), String::new()),
    };
    format!(
        "<!doctype html>\n<html>\n<head><meta charset=\"utf-8\">{head}</head>\n\
         <body><div id=\"app\">{body}</div>{script}{CLIENT}</body>\n</html>\n"
    )
}

/// Serve a file from the built client bundle dir at `/@oj/<path>`.
async fn serve_client_asset(
    State(state): State<Arc<SsrState>>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(client) = &state.client else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Confine to the bundle dir: reject any traversal in the request path.
    if path.split('/').any(|c| c == "..") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let file = client.dir.join(&path);
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let ct = if path.ends_with(".css") { "text/css" } else { "text/javascript" };
            ([(header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn error_page(msg: &str) -> Response {
    Html(format!(
        "<!doctype html><html><body><pre style=\"color:#c00;white-space:pre-wrap\">[oj ssr] {}</pre>{CLIENT}</body></html>",
        html_escape(msg)
    ))
    .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Stem of an entry path, falling back to `default` (e.g. "entry-server").
fn stem_of(entry: &str, default: &str) -> String {
    std::path::Path::new(entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(default)
        .to_string()
}

/// Derive the client hydration entry from the server entry by convention:
/// swap "server" -> "client" in the filename and return it if that file
/// exists (root-relative), else None.
fn derive_client_entry(root: &std::path::Path, server_entry: &str) -> Option<String> {
    let file = std::path::Path::new(server_entry).file_name()?.to_str()?;
    if !file.contains("server") {
        return None;
    }
    let client_file = file.replace("server", "client");
    let client_rel = match std::path::Path::new(server_entry).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            format!("{}/{}", dir.to_string_lossy(), client_file)
        }
        _ => client_file,
    };
    root.join(&client_rel).is_file().then_some(client_rel)
}

async fn ws(State(state): State<Arc<SsrState>>, up: WebSocketUpgrade) -> impl IntoResponse {
    up.on_upgrade(move |mut socket| async move {
        let mut rx = state.reload_tx.subscribe();
        while rx.recv().await.is_ok() {
            if socket.send(Message::Text("reload".into())).await.is_err() {
                break;
            }
        }
    })
}

fn spawn_watcher(state: Arc<SsrState>) {
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut watcher) = notify::recommended_watcher(tx) else { return };
        if watcher.watch(&state.root, RecursiveMode::Recursive).is_err() {
            return;
        }
        for event in rx.into_iter().flatten() {
            let relevant = event.paths.iter().any(|p| {
                !p.components().any(|c| {
                    let c = c.as_os_str();
                    c == "node_modules" || c == ".oj-cache"
                }) && p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "ts" | "tsx" | "js" | "jsx" | "css" | "scss"))
            });
            if relevant {
                state.dirty.store(true, Ordering::SeqCst);
                let _ = state.reload_tx.send(());
            }
        }
    });
}
