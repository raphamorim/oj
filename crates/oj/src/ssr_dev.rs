// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev-server SSR (`oj dev --ssr <entry>`).
//!
//! Build-and-run SSR: on each request the SSR bundle is (re)built if a source
//! file changed, then executed in Node to produce HTML; a WebSocket triggers a
//! full page reload on edit. This is the pre-module-runner style of dev SSR —
//! honest and useful, but coarse: it rebuilds the whole bundle per change and
//! spawns Node per render. Fine-grained SSR HMR and the Environment API module
//! runner are the larger remaining work.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context;
use axum::{
    Router,
    extract::{State, WebSocketUpgrade, ws::Message},
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
    bundle_dir: PathBuf,
    stem: String,
    dirty: AtomicBool,
    version: AtomicU64,
    reload_tx: broadcast::Sender<()>,
}

pub async fn ssr_dev(root: PathBuf, entry: String, port: u16) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;
    let stem = std::path::Path::new(&entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("server")
        .to_string();
    let (reload_tx, _) = broadcast::channel(16);
    let state = Arc::new(SsrState {
        bundle_dir: root.join(".oj-cache").join("ssr"),
        root,
        entry,
        stem,
        dirty: AtomicBool::new(true),
        version: AtomicU64::new(0),
        reload_tx,
    });

    spawn_watcher(Arc::clone(&state));

    let app = Router::new()
        .route("/__ojssr", get(ws))
        .fallback(get(render))
        .with_state(Arc::clone(&state));
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("  oj dev (ssr)");
    println!("  entry: {}", state.entry);
    println!("  http://localhost:{port}/");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn render(State(state): State<Arc<SsrState>>) -> Response {
    // Rebuild the SSR bundle if a source file changed since the last render.
    if state.dirty.swap(false, Ordering::SeqCst) {
        if let Err(e) =
            crate::build::build_ssr(&state.root, &state.bundle_dir, &state.entry, false).await
        {
            return error_page(&format!("SSR build failed:\n{e}"));
        }
    }
    let bundle = state.bundle_dir.join(format!("{}.mjs", state.stem));
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
            let body = String::from_utf8_lossy(&o.stdout);
            Html(format!(
                "<!doctype html>\n<html>\n<head><meta charset=\"utf-8\"></head>\n\
                 <body><div id=\"app\">{body}</div>{CLIENT}</body>\n</html>\n"
            ))
            .into_response()
        }
        Ok(o) => error_page(&format!("SSR render failed:\n{}", String::from_utf8_lossy(&o.stderr))),
        Err(e) => error_page(&format!("could not spawn node: {e}")),
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
