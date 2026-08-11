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
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

struct Runner {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    _child: Child,
}

struct StartState {
    proxy_prefixes: Vec<String>,
    runner: Arc<tokio::sync::Mutex<Runner>>,
}

pub async fn start_dev(root: PathBuf, port: Option<u16>) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = root.join(".oj-cache").join("start");
    oj_server::write_start_assets(&cache)?;
    generate_route_tree(&root, &cache)?;

    // Reuse the dev server for module/asset serving; the document route sits on top.
    let built = oj_server::DevServer { root: root.clone(), port, bundle: false }.build_app().await?;
    let runner = spawn_start_runner(&root, &cache).await?;
    let state = Arc::new(StartState {
        proxy_prefixes: built.proxy_prefixes.clone(),
        runner: Arc::new(tokio::sync::Mutex::new(runner)),
    });

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

/// Run `@tanstack/router-generator` once to write `src/routeTree.gen.ts`.
fn generate_route_tree(root: &Path, cache: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("node")
        .arg(cache.join("generate.mjs"))
        .env("OJ_APP_ROOT", root)
        .current_dir(root)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run route generator (node): {e}"))?;
    if !status.success() {
        anyhow::bail!("route tree generation failed");
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
    // The client hydration entry is not wired yet (browser-side alias
    // resolution is a later slice); serve an inert module so it does not 404.
    if req.uri().path() == "/@oj-start/client-entry.js" {
        return (
            [(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")],
            "// oj: client hydration not wired yet\nexport {};\n",
        )
            .into_response();
    }
    match classify(&req, &state.proxy_prefixes) {
        Route::Document => render_document(&state, req.uri().path()).await,
        Route::Pass => next.run(req).await,
    }
}

/// Send `{url}` to the runner and return the rendered document. Detached so a
/// cancelled request still drains the runner's one-line reply.
async fn render_document(state: &StartState, path: &str) -> Response {
    let runner = Arc::clone(&state.runner);
    let url = path.to_owned();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut guard = runner.lock_owned().await;
        let result = async {
            let cmd = serde_json::json!({ "url": url });
            guard.stdin.write_all(format!("{cmd}\n").as_bytes()).await.map_err(|e| e.to_string())?;
            guard.stdin.flush().await.map_err(|e| e.to_string())?;
            let line = guard
                .lines
                .next_line()
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "start runner closed".to_string())?;
            let v: serde_json::Value =
                serde_json::from_str(&line).map_err(|e| e.to_string())?;
            let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(200) as u16;
            let body = v.get("body").and_then(|b| b.as_str()).unwrap_or("").to_owned();
            Ok::<_, String>((status, body))
        }
        .await;
        let _ = tx.send(result);
    });
    match rx.await.unwrap_or_else(|_| Err("start runner task cancelled".to_string())) {
        Ok((status, body)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            Html(body),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {e}")).into_response(),
    }
}
